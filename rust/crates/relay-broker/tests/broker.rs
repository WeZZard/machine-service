//! TL-2: the broker with real remote sessions on loopback, real relay agent
//! cores dialling it, and a test client that follows the relay protocol
//! (BK-1 to BK-3, RP-2 to RP-10).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use relay_agent::protocol::{ExecutionState, Op, OpKind, Outcome};
use relay_agent::session_link::DialOptions;
use relay_broker::tls::Authority;
use relay_broker::{Role, RunError, Target, Upload};
use remote_session::RejectCode;

const WAIT: Duration = Duration::from_secs(20);

fn rc(outcome: &Outcome) -> Option<i32> {
    match outcome {
        Outcome::Exit { rc, .. } => *rc,
        other => panic!("not an exit: {other:?}"),
    }
}

fn failed_code(outcome: &Outcome) -> (&str, bool) {
    match outcome {
        Outcome::Failed {
            code, committed, ..
        } => (code.as_str(), *committed),
        other => panic!("not a failure: {other:?}"),
    }
}

#[test]
fn a_client_reaches_only_its_own_leases_agent() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let b = lease(&h.broker, "vm-b");
    let agent_a = TestAgent::start(&h.url("ws"), &a.agent, None);
    let agent_b = TestAgent::start(&h.url("ws"), &b.agent, None);
    std::fs::write(agent_a.home.join("who"), "a\n").unwrap();
    std::fs::write(agent_b.home.join("who"), "b\n").unwrap();
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    assert!(h.broker.wait_for_agent(&b.target, WAIT).is_some());

    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    assert_eq!(client.ready.lease, "vm-a");
    let agent = client.ready.agent.clone().expect("the agent is connected");
    assert_eq!(agent.build, "test-build");
    assert_eq!(agent.os, std::env::consts::OS);

    client.call(1, "x-1", exec("cat who"), b"");
    let answer = client.wait(1, WAIT);
    let result = answer.result.clone().unwrap();
    assert_eq!(rc(&result.outcome), Some(0));
    assert!(!result.replayed);
    assert_eq!(answer.bytes, b"a\n");
    assert!(answer.end_matches());

    // The same execution ID on another lease is another operation.
    let other = TestClient::connect(&h.url("ws"), &b.client).unwrap();
    other.call(7, "x-1", exec("cat who"), b"");
    let answer = other.wait(7, WAIT);
    assert_eq!(answer.bytes, b"b\n");
    assert!(!answer.result.unwrap().replayed);
    drop((agent_a, agent_b));
}

#[test]
fn credentials_roles_and_leases_are_checked_on_every_hello() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let rejected = |config| match TestClient::connect_with(config) {
        Err(remote_session::Error::Rejected { code, .. }) => code,
        Err(other) => panic!("expected a rejection, got {other}"),
        Ok(_) => panic!("expected a rejection"),
    };
    assert_eq!(
        rejected(client_config(&h.url("ws"), "not-a-credential")),
        RejectCode::Unauthorized
    );
    // An agent credential is not a client credential, and the other way.
    assert_eq!(
        rejected(client_config(&h.url("ws"), &a.agent)),
        RejectCode::Unauthorized
    );
    let mut as_agent = client_config(&h.url("ws"), &a.client);
    as_agent.attributes = serde_json::json!({"role": "agent"});
    assert_eq!(rejected(as_agent), RejectCode::Unauthorized);
    let mut no_role = client_config(&h.url("ws"), &a.client);
    no_role.attributes = serde_json::json!({});
    assert_eq!(rejected(no_role), RejectCode::Unauthorized);
    let mut other_lease = client_config(&h.url("ws"), &a.client);
    other_lease.attributes = serde_json::json!({"role": "client", "lease": "vm-b"});
    assert_eq!(rejected(other_lease), RejectCode::Unauthorized);
    let mut named = client_config(&h.url("ws"), &a.client);
    named.attributes = serde_json::json!({"role": "client", "lease": "vm-a"});
    assert!(TestClient::connect_with(named).is_ok());
    // Only hashes are kept.
    assert!(h.broker.has_credential(&a.target, Role::Client));
}

#[test]
fn a_repeated_execution_id_never_runs_twice() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    let command = "echo run >> runs; echo out-$(wc -l < runs | tr -d ' ')";
    client.call(1, "once", exec(command), b"");
    let first = client.wait(1, WAIT);
    assert_eq!(first.bytes, b"out-1\n");
    assert!(!first.result.as_ref().unwrap().replayed);
    // Again on the same session, on another session, and in process.
    client.call(2, "once", exec(command), b"");
    let second = client.wait(2, WAIT);
    assert_eq!(second.bytes, b"out-1\n", "the output is replayed whole");
    assert!(second.result.as_ref().unwrap().replayed);
    let again = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    again.call(1, "once", exec(command), b"");
    assert_eq!(again.wait(1, WAIT).bytes, b"out-1\n");
    let answer = h
        .broker
        .run(&a.target, "once", exec(command), Upload::None, WAIT)
        .unwrap();
    assert!(answer.replayed && answer.end_matches());
    assert_eq!(answer.bytes, b"out-1\n");
    // The command ran once.
    client.call(3, "count", exec("wc -l < runs | tr -d ' '"), b"");
    assert_eq!(client.wait(3, WAIT).bytes, b"1\n");
}

#[test]
fn a_short_action_costs_one_round_trip_of_the_open_session() {
    // DC-194: no new connection, hello, lookup or wait for an acknowledgement
    // stands between a call and its result, on either session.
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    for call in 1..=20 {
        let before = client.session.counters();
        client.call(call, &format!("short-{call}"), exec("echo ok"), b"");
        assert_eq!(client.wait(call, WAIT).bytes, b"ok\n");
        let after = client.session.counters();
        assert_eq!(after.round_trips_since(&before), 1, "call {call}");
        assert_eq!(after.connections, 1);
    }
    // The broker acknowledges each call on receipt, so the client measures
    // the path's round trip from it.
    let rtt = client.session.counters().rtt;
    assert!(rtt.samples >= 20, "{rtt:?}");
    assert!(rtt.min.unwrap() < Duration::from_secs(1), "{rtt:?}");
}

#[test]
fn a_late_caller_attaches_to_a_running_command() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(1, "slow", exec("echo start; sleep 1; echo stop"), b"");
    assert!(wait_until(WAIT, || client.seen(1).bytes == b"start\n"));
    let late = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    late.query(1, "slow");
    assert_eq!(
        late.wait(1, WAIT).query,
        Some(ExecutionState::Running { op: OpKind::Exec })
    );
    late.call(2, "slow", exec("ignored"), b"");
    let attached = late.wait(2, WAIT);
    assert_eq!(attached.bytes, b"start\nstop\n");
    assert!(attached.result.unwrap().replayed);
    let first = client.wait(1, WAIT);
    assert_eq!(first.bytes, b"start\nstop\n");
    assert!(!first.result.unwrap().replayed);
}

#[test]
fn a_call_waits_for_an_agent_that_registers_late() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    assert!(client.ready.agent.is_none());
    client.call(1, "early", exec("echo hi"), b"");
    assert!(wait_until(WAIT, || !client.seen(1).pending.is_empty()));
    let pending = client.seen(1).pending[0].clone();
    assert_eq!(pending.code, "agent_not_connected");
    assert!(pending.wait_ms > 0 && pending.wait_ms <= 10_000);
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let answer = client.wait(1, WAIT);
    assert_eq!(answer.bytes, b"hi\n");
    assert_eq!(rc(&answer.result.unwrap().outcome), Some(0));
}

#[test]
fn a_call_ends_agent_not_connected_and_may_be_sent_again() {
    let h = Harness::start(Options {
        agent_wait: Duration::from_millis(500),
        ..Options::default()
    });
    let a = lease(&h.broker, "vm-a");
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(1, "x", exec("echo ran >> ran"), b"");
    let answer = client.wait(1, WAIT);
    assert_eq!(answer.pending.len(), 1);
    assert_eq!(
        failed_code(&answer.result.unwrap().outcome),
        ("agent_not_connected", false)
    );
    client.query(2, "x");
    assert_eq!(client.wait(2, WAIT).query, Some(ExecutionState::Unknown));
    // Nothing ran and nothing was recorded, so the same ID runs once now.
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    client.call(3, "x", exec("echo ran >> ran; cat ran"), b"");
    let answer = client.wait(3, WAIT);
    assert_eq!(answer.bytes, b"ran\n");
    assert!(!answer.result.unwrap().replayed);
    drop(agent);
}

#[test]
fn revocation_ends_sessions_and_refuses_every_credential() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(1, "long", exec("sleep 30"), b"");
    assert!(wait_until(WAIT, || matches!(
        h.broker.query(&a.target, "long"),
        ExecutionState::Running { .. }
    )));
    h.broker.revoke(&a.target, "released");
    assert!(client.wait_end(WAIT), "the client session ends");
    assert_eq!(client.revoked.lock().unwrap().as_deref(), Some("released"));
    assert!(matches!(
        TestClient::connect(&h.url("ws"), &a.client),
        Err(remote_session::Error::Rejected {
            code: RejectCode::Unauthorized,
            ..
        })
    ));
    assert!(wait_until(WAIT, || h.broker.agent(&a.target).is_none()));
    // The agent dials again and is refused.
    std::thread::sleep(Duration::from_secs(1));
    assert!(h.broker.agent(&a.target).is_none());
    assert!(!h.broker.has_credential(&a.target, Role::Agent));
    let answer = h
        .broker
        .run(&a.target, "after", exec("true"), Upload::None, WAIT)
        .unwrap();
    assert_eq!(failed_code(&answer.outcome), ("revoked", false));
}

#[test]
fn outcomes_survive_a_broker_restart() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let url = h.url("ws");
    let _agent = TestAgent::start_in(&home, &url, &a.agent, None, DialOptions::default());
    let client = TestClient::connect(&url, &a.client).unwrap();
    client.call(1, "before", exec("echo once >> log; echo done"), b"");
    assert_eq!(client.wait(1, WAIT).bytes, b"done\n");
    client.call(
        2,
        "spanning",
        exec("sleep 2; echo late >> log; echo late"),
        b"",
    );
    assert!(wait_until(WAIT, || home.join("log").exists()));
    let port = h.crash();

    // A new broker on the same port knows nothing; the lease's hashes are
    // admitted again, as machine-service does from its lease records.
    let h = Harness::start(Options {
        port,
        ..Options::default()
    });
    let target = Target::Lease("vm-a".into());
    h.broker.admit(
        &target,
        Role::Agent,
        &relay_broker::credential_sha256(&a.agent),
        None,
    );
    h.broker.admit(
        &target,
        Role::Client,
        &relay_broker::credential_sha256(&a.client),
        None,
    );
    assert!(
        h.broker.wait_for_agent(&target, WAIT).is_some(),
        "the agent dials a new session"
    );
    assert!(client.wait_end(WAIT), "the old client session is gone");
    let client = TestClient::connect(&url, &a.client).unwrap();
    client.query(1, "before");
    match client.wait(1, WAIT).query.unwrap() {
        ExecutionState::Completed { op, outcome } => {
            assert_eq!(op, OpKind::Exec);
            assert_eq!(rc(&outcome), Some(0));
        }
        other => panic!("expected completed, got {other:?}"),
    }
    client.query(2, "never-sent");
    assert_eq!(client.wait(2, WAIT).query, Some(ExecutionState::Unknown));
    // Resending both IDs runs neither again.
    client.call(3, "before", exec("echo once >> log; echo done"), b"");
    let replay = client.wait(3, WAIT);
    assert_eq!(replay.bytes, b"done\n");
    assert!(replay.result.unwrap().replayed);
    client.call(
        4,
        "spanning",
        exec("sleep 2; echo late >> log; echo late"),
        b"",
    );
    let spanning = client.wait(4, WAIT);
    assert_eq!(spanning.bytes, b"late\n");
    assert_eq!(
        std::fs::read_to_string(home.join("log")).unwrap(),
        "once\nlate\n"
    );
}

#[test]
fn a_call_out_when_the_agent_session_ends_is_sent_again_and_runs_once() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(
        1,
        "spans",
        exec("echo a; echo ran >> ran; sleep 1; echo b"),
        b"",
    );
    assert!(wait_until(WAIT, || client.seen(1).bytes == b"a\n"));
    assert!(h.broker.end_agent_session(&a.target));
    let answer = client.wait(1, WAIT);
    assert_eq!(answer.bytes, b"a\nb\n", "no byte is delivered twice");
    assert!(answer.end_matches());
    let result = answer.result.unwrap();
    assert!(!result.replayed, "the origin's own run, answered again");
    assert_eq!(rc(&result.outcome), Some(0));
    assert_eq!(
        std::fs::read_to_string(agent.home.join("ran")).unwrap(),
        "ran\n"
    );
    assert!(h.logged("sending it again"));
}

#[test]
fn uploads_are_checked_and_files_round_trip() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    // 3 MiB crosses several chunks each way.
    let bytes: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let write = Op::Write {
        path: "dir/file.bin".into(),
        name: String::new(),
        mode: Some(0o600),
    };
    std::fs::create_dir_all(agent.home.join("dir")).unwrap();
    client.call(1, "w", write.clone(), &bytes);
    match client.wait(1, WAIT).result.unwrap().outcome {
        Outcome::Written(digest) => assert_eq!(digest.size, bytes.len() as u64),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read(agent.home.join("dir/file.bin")).unwrap(),
        bytes
    );
    client.call(
        2,
        "r",
        Op::Read {
            path: "dir/file.bin".into(),
        },
        b"",
    );
    let read = client.wait(2, WAIT);
    assert_eq!(read.bytes, bytes);
    assert!(read.end_matches());
    // A repeated read ID answers its end without the bytes (RP-7).
    client.call(
        3,
        "r",
        Op::Read {
            path: "dir/file.bin".into(),
        },
        b"",
    );
    let again = client.wait(3, WAIT);
    assert!(again.bytes.is_empty() && again.end.is_some());
    assert!(again.result.unwrap().replayed);
    // An upload that does not match its end is refused before it is handed on.
    client.send(&relay_agent::protocol::Message::Call(
        relay_agent::protocol::Call {
            lease: None,
            resume: false,
            offset: 0,
            call: 4,
            execution: "bad".into(),
            op: write,
        },
    ));
    client.send(&relay_agent::protocol::Message::Chunk {
        call: 4,
        data: b"abc".to_vec(),
    });
    client.send(&relay_agent::protocol::Message::End {
        call: 4,
        execution: "bad".into(),
        size: 3,
        sha256: "0".repeat(64),
    });
    assert_eq!(
        failed_code(&client.wait(4, WAIT).result.unwrap().outcome),
        ("checksum_mismatch", false)
    );
    // Malformed calls are refused under their own number.
    client.call(5, "", exec("true"), b"");
    assert_eq!(
        failed_code(&client.wait(5, WAIT).result.unwrap().outcome),
        ("bad_request", false)
    );
    // Script and status pass through.
    client.call(
        6,
        "s",
        Op::Script { timeout_s: 30.0 },
        b"echo from-script\n",
    );
    assert_eq!(client.wait(6, WAIT).bytes, b"from-script\n");
    client.call(7, "st", Op::Status, b"");
    assert!(matches!(
        client.wait(7, WAIT).result.unwrap().outcome,
        Outcome::Status(_)
    ));
}

/// RP-9, RP-15: a call may end before its stream does, so a chunk or an
/// end for a call that is not open is dropped, and the session carries on.
#[test]
fn a_chunk_for_a_call_that_is_not_open_is_dropped() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.send(&relay_agent::protocol::Message::Chunk {
        call: 9,
        data: b"x".to_vec(),
    });
    client.send(&relay_agent::protocol::Message::End {
        call: 9,
        execution: "none".into(),
        size: 1,
        sha256: guest_actions::sha256_hex(b"x"),
    });
    client.call(1, "after-stray", exec("echo on"), b"");
    assert_eq!(client.wait(1, WAIT).bytes, b"on\n");
}

#[test]
fn the_pinned_authority_serves_by_name_and_by_address() {
    let dir = tempfile::tempdir().unwrap();
    let authority = Arc::new(Authority::load_or_create(&dir.path().join("tls")).unwrap());
    // It is kept: loading again gives the same authority.
    let again = Authority::load_or_create(&dir.path().join("tls")).unwrap();
    assert_eq!(again.sha256(), authority.sha256());
    assert_eq!(
        relay_broker::tls::pem_sha256(authority.cert_pem()).as_deref(),
        Some(authority.sha256())
    );
    let h = Harness::start(Options {
        tls: Some(Arc::clone(&authority)),
        ..Options::default()
    });
    let a = lease(&h.broker, "vm-a");
    let trust = remote_session::tls::client_config(&[authority.cert_pem().as_bytes()]).unwrap();
    let port = h.addr.port();

    // By a name only a resolver knows (TC-6), by `localhost`, and by address.
    let resolver =
        remote_session::fixed_resolver(&format!("broker.example.test:{port}:127.0.0.1")).unwrap();
    for (url, resolver) in [
        (
            format!("wss://broker.example.test:{port}/relay"),
            Some(resolver),
        ),
        (format!("wss://localhost:{port}/relay"), None),
        (format!("wss://127.0.0.1:{port}/relay"), None),
    ] {
        let mut config = client_config(&url, &a.client);
        config.tls = Some(trust.clone());
        config.resolver = resolver;
        let client = TestClient::connect_with(config).unwrap_or_else(|e| panic!("{url}: {e}"));
        assert_eq!(client.ready.lease, "vm-a");
    }
    // The agent trusts the authority from its configuration.
    let _agent = TestAgent::start(
        &format!("wss://localhost:{port}/relay"),
        &a.agent,
        Some(authority.cert_pem().to_string()),
    );
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    // Without the authority, the public roots alone do not trust it.
    let mut untrusting = client_config(&format!("wss://localhost:{port}/relay"), &a.client);
    untrusting.timing.handshake_timeout = Duration::from_secs(2);
    assert!(TestClient::connect_with(untrusting).is_err());
}

#[test]
fn an_in_process_run_times_out_without_losing_the_outcome() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    assert_eq!(
        h.broker.run(
            &a.target,
            "t",
            exec("sleep 1; echo ok"),
            Upload::None,
            Duration::from_millis(100)
        ),
        Err(RunError::Timeout)
    );
    assert!(wait_until(WAIT, || matches!(
        h.broker.query(&a.target, "t"),
        ExecutionState::Completed { .. }
    )));
    let answer = h
        .broker
        .run(&a.target, "t", exec("sleep 1; echo ok"), Upload::None, WAIT)
        .unwrap();
    assert_eq!(answer.bytes, b"ok\n");
    assert!(answer.replayed);
}

/// RP-13: a client moves a folder both ways as a tar stream; a repeated
/// folder read answers its end without the bytes, like a read.
#[test]
fn folders_round_trip_as_tar_streams() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("source");
    std::fs::create_dir_all(source.join("n")).unwrap();
    let big: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i * 3 % 251) as u8).collect();
    std::fs::write(source.join("n/big.bin"), &big).unwrap();
    let stream = byte_transfer::pack_into(&source, Vec::new()).unwrap();
    client.call(
        1,
        "tw",
        Op::WriteTree {
            path: "landed".into(),
            name: String::new(),
        },
        &stream,
    );
    match client.wait(1, WAIT).result.unwrap().outcome {
        Outcome::Written(digest) => assert_eq!(digest.size, stream.len() as u64),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read(agent.home.join("landed/n/big.bin")).unwrap(),
        big
    );
    for (call, replayed) in [(2, false), (3, true)] {
        client.call(
            call,
            "tr",
            Op::ReadTree {
                path: "landed".into(),
            },
            b"",
        );
        let read = client.wait(call, WAIT);
        assert_eq!(read.result.as_ref().unwrap().replayed, replayed);
        if replayed {
            assert!(read.bytes.is_empty() && read.end.is_some());
        } else {
            assert!(read.end_matches());
            let archive = scratch.path().join("back.tar");
            std::fs::write(&archive, &read.bytes).unwrap();
            byte_transfer::unpack(&archive, &scratch.path().join("back")).unwrap();
            assert_eq!(
                std::fs::read(scratch.path().join("back/n/big.bin")).unwrap(),
                big
            );
        }
    }
}

/// RA-6: a restart of the relay agent waits until no call is out, so the
/// call that runs completes; a call made during the restart goes to the
/// agent's next session; the agent's records persist.
#[test]
fn a_restart_waits_for_running_calls_and_hands_later_ones_to_the_next_session() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    assert!(wait_until(WAIT, || h.broker.agent(&a.target).is_some()));
    let restarts = Arc::new(AtomicUsize::new(0));
    {
        let restarts = Arc::clone(&restarts);
        let same = agent.agent.clone();
        // The in-process agent cannot replace its process; it ends its
        // session instead, after a moment, and the dialer registers anew.
        agent.agent.set_restart(move || {
            restarts.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(500));
            same.drop_links();
            std::io::Error::other("restarted in place")
        });
    }
    client.call(1, "slow", exec("sleep 2; echo done"), b"");
    assert!(wait_until(WAIT, || matches!(
        h.broker.query(&a.target, "slow"),
        ExecutionState::Running { .. }
    )));
    let broker = h.broker.clone();
    let target = a.target.clone();
    let restarting = std::thread::spawn(move || {
        broker.restart_agent(&target, Duration::from_secs(30), Duration::from_secs(30))
    });
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(restarts.load(Ordering::SeqCst), 0, "not while a call runs");
    let slow = client.wait(1, WAIT);
    assert_eq!(slow.bytes, b"done\n");
    assert!(matches!(
        slow.result.unwrap().outcome,
        Outcome::Exit { rc: Some(0), .. }
    ));
    assert!(wait_until(WAIT, || restarts.load(Ordering::SeqCst) == 1));
    client.call(2, "during", exec("echo during"), b"");
    let back = restarting.join().unwrap();
    assert!(back.is_ok(), "{back:?}");
    let during = client.wait(2, WAIT);
    assert_eq!(during.bytes, b"during\n");
    assert!(h.logged("relay agent restarting"));
    client.call(3, "slow", exec("never"), b"");
    let replay = client.wait(3, WAIT);
    assert!(replay.result.unwrap().replayed);
    assert_eq!(replay.bytes, b"done\n");
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
}

/// RA-6: an agent that cannot restart, or one with no session, says so and
/// keeps serving.
#[test]
fn a_restart_that_cannot_happen_releases_the_hold() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    assert_eq!(
        h.broker.restart_agent(
            &a.target,
            Duration::from_millis(100),
            Duration::from_secs(1)
        ),
        Err(relay_broker::RestartError::Unavailable)
    );
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(wait_until(WAIT, || h.broker.agent(&a.target).is_some()));
    let refused = h
        .broker
        .restart_agent(&a.target, Duration::from_secs(5), Duration::from_secs(5));
    assert!(
        matches!(&refused, Err(relay_broker::RestartError::Refused(why)) if why.starts_with("unsupported")),
        "{refused:?}"
    );
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(1, "after", exec("echo after"), b"");
    assert_eq!(client.wait(1, WAIT).bytes, b"after\n");
}

/// DC-173: the broker names a bound lease on every call it hands on, and a
/// purge removes that lease's records from the relay agent and from the
/// broker, leaving the next lease's records alone.
#[test]
fn a_purge_removes_one_leases_records_from_the_agent_and_the_broker() {
    let h = Harness::start(Options::default());
    let target = Target::Durable("mac".into());
    let credential = h.broker.issue(&target, Role::Agent, None);
    let agent = TestAgent::start(&h.url("ws"), &credential, None);
    assert!(h.broker.wait_for_agent(&target, WAIT).is_some());
    let run = |execution: &str| {
        h.broker
            .run(&target, execution, exec("echo out"), Upload::None, WAIT)
            .unwrap()
    };

    h.broker.bind_lease(&target, "lease-1", false);
    assert!(!h.broker.lease_reached_agent(&target, "lease-1"));
    assert_eq!(run("first").bytes, b"out\n");
    assert!(h.broker.lease_reached_agent(&target, "lease-1"));
    h.broker.bind_lease(&target, "lease-2", false);
    assert_eq!(run("second").bytes, b"out\n");

    let purged = h.broker.purge(&target, "lease-1", WAIT).unwrap();
    assert_eq!(
        purged,
        relay_broker::Purged {
            records: 1,
            bytes: 4
        }
    );
    // Neither the broker nor the agent knows the purged ID any more.
    assert_eq!(h.broker.query(&target, "first"), ExecutionState::Unknown);
    assert_eq!(agent.agent.query("first"), ExecutionState::Unknown);
    assert!(matches!(
        agent.agent.query("second"),
        ExecutionState::Completed { .. }
    ));
    assert!(h.logged("purged 1 records (4 bytes) of lease lease-1"));
}

/// DC-173: a client never purges; only machine-service does, at release.
#[test]
fn a_client_cannot_purge() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let _agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();
    client.call(
        1,
        "p",
        Op::Purge {
            lease: "vm-a".into(),
        },
        b"",
    );
    let answer = client.wait(1, WAIT);
    assert_eq!(
        failed_code(&answer.result.unwrap().outcome),
        ("forbidden", false)
    );
}

/// DC-173: without an agent that answers, a purge fails within its bound
/// and says why, and a revoked lease's purge fails at once.
#[test]
fn a_purge_the_agent_cannot_confirm_fails_within_its_bound() {
    let h = Harness::start(Options::default());
    let target = Target::Durable("mac".into());
    h.broker.issue(&target, Role::Agent, None);
    h.broker.bind_lease(&target, "lease-1", true);
    let begun = std::time::Instant::now();
    let refused = h
        .broker
        .purge(&target, "lease-1", Duration::from_millis(500))
        .unwrap_err();
    assert!(refused.contains("no session"), "{refused}");
    assert!(begun.elapsed() < Duration::from_secs(5));

    let a = lease(&h.broker, "vm-a");
    h.broker.revoke(&a.target, "released");
    let refused = h.broker.purge(&a.target, "vm-a", WAIT).unwrap_err();
    assert!(refused.contains("released"), "{refused}");
}
