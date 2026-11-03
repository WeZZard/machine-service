//! TL-2: resumable streams through the broker (`docs/arch/relay-protocol.md`
//! RP-15): uploads of protocol-2 clients pass through within a window and
//! continue from the agent's committed offset after a lost client session,
//! a lost agent session or a restart of the broker; reads start at any
//! offset; and a short or corrupted stream never commits.

mod common;

use std::time::Duration;

use common::*;
use relay_agent::protocol::{ExecutionState, Op, Outcome, MAX_CHUNK, UPLOAD_WINDOW};
use relay_agent::session_link::DialOptions;
use relay_broker::{Role, Target, Upload};

const WAIT: Duration = Duration::from_secs(30);

fn pattern(size: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn write(path: &str) -> Op {
    Op::Write {
        path: path.into(),
        name: String::new(),
        mode: None,
    }
}

fn code(outcome: &Outcome) -> Option<(&str, bool)> {
    match outcome {
        Outcome::Failed {
            code, committed, ..
        } => Some((code.as_str(), *committed)),
        _ => None,
    }
}

fn client_v2(url: &str, credential: &str) -> TestClient {
    TestClient::connect_with(client_config_v2(url, credential)).unwrap()
}

/// RP-15: a protocol-2 upload streams through, and the broker reports the
/// agent's progress to the client.
#[test]
fn an_upload_streams_through_with_the_agents_progress() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = client_v2(&h.url("ws"), &a.client);
    // The broker announces its own protocol; protocol 3 changes only the
    // agent's hello, so the client keeps protocol 2's streams.
    assert!(client.ready.protocol >= 2);
    let whole = pattern(UPLOAD_WINDOW as usize + 3 * MAX_CHUNK + 5, 1);
    let streamed = client.stream(1, "up", write("up.bin"), &whole[..], false, None);
    assert_eq!(streamed, Streamed::Sent { from: 0 });
    let answer = client.wait(1, WAIT);
    assert!(
        matches!(answer.result.unwrap().outcome, Outcome::Written(d) if d.size == whole.len() as u64)
    );
    assert_eq!(answer.progress.first(), Some(&0));
    assert!(answer.progress.len() > 2, "{:?}", answer.progress);
    assert_eq!(std::fs::read(agent.home.join("up.bin")).unwrap(), whole);
    assert!(h.broker.held_most() <= UPLOAD_WINDOW + 2 * MAX_CHUNK as u64);
}

/// RP-15 (TL-2): a client whose session ends during an upload, at any
/// offset, continues it in a new session from the agent's committed offset.
#[test]
fn an_upload_continues_after_the_client_session_ends() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let whole = pattern(4 * MAX_CHUNK + 999, 2);
    for (index, cut) in [1u64, 4096, MAX_CHUNK as u64, 3 * MAX_CHUNK as u64 + 17]
        .into_iter()
        .enumerate()
    {
        let execution = format!("client-loss-{index}");
        let path = format!("c{index}.bin");
        let client = client_v2(&h.url("ws"), &a.client);
        let streamed = client.stream(1, &execution, write(&path), &whole[..], false, Some(cut));
        assert_eq!(streamed, Streamed::Stopped { from: 0, at: cut });
        client.session.flush(WAIT).unwrap();
        client.session.close();
        assert!(wait_until(WAIT, || agent.agent.upload_held(&execution) == Some(cut)));
        assert!(!agent.home.join(&path).exists(), "nothing visible yet");

        let client = client_v2(&h.url("ws"), &a.client);
        let streamed = client.stream(1, &execution, write(&path), &whole[..], true, None);
        assert_eq!(streamed, Streamed::Sent { from: cut });
        let result = client.wait(1, WAIT).result.unwrap();
        assert!(!result.replayed);
        assert!(matches!(result.outcome, Outcome::Written(d) if d.size == whole.len() as u64));
        assert_eq!(std::fs::read(agent.home.join(&path)).unwrap(), whole);
        // A resume after the commit replays the result.
        let streamed = client.stream(2, &execution, write(&path), &whole[..], true, None);
        assert_eq!(streamed, Streamed::Answered { from: 0 });
        assert!(client.wait(2, WAIT).result.unwrap().replayed);
    }
}

/// RP-15 (TL-2): when the agent's session ends during a client's upload,
/// the client hears `upload_incomplete` and continues from the agent's
/// committed offset once the agent is back.
#[test]
fn an_upload_continues_after_the_agent_session_ends() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let client = client_v2(&h.url("ws"), &a.client);
    let whole = pattern(5 * MAX_CHUNK + 3, 3);
    let cut = 2 * MAX_CHUNK as u64 + 123;
    client.stream(
        1,
        "agent-loss",
        write("a.bin"),
        &whole[..],
        false,
        Some(cut),
    );
    assert!(wait_until(WAIT, || agent.agent.upload_held("agent-loss") == Some(cut)));
    assert!(h.broker.end_agent_session(&a.target));
    let failed = client.wait(1, WAIT).result.unwrap();
    assert_eq!(code(&failed.outcome), Some(("upload_incomplete", false)));
    let streamed = client.stream(2, "agent-loss", write("a.bin"), &whole[..], true, None);
    assert_eq!(streamed, Streamed::Sent { from: cut });
    let result = client.wait(2, WAIT).result.unwrap();
    assert!(matches!(result.outcome, Outcome::Written(d) if d.size == whole.len() as u64));
    assert_eq!(std::fs::read(agent.home.join("a.bin")).unwrap(), whole);
}

/// RP-15 (TL-2): an upload continues after a restart of the broker: the
/// agent kept its bytes, and the client resumes in a new session.
#[test]
fn an_upload_continues_after_a_broker_restart() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let url = h.url("ws");
    let agent = TestAgent::start_in(&home, &url, &a.agent, None, DialOptions::default());
    let client = client_v2(&url, &a.client);
    let whole = pattern(3 * MAX_CHUNK + 77, 4);
    let cut = MAX_CHUNK as u64 + 1;
    client.stream(1, "restart", write("r.bin"), &whole[..], false, Some(cut));
    client.session.flush(WAIT).unwrap();
    assert!(wait_until(WAIT, || agent.agent.upload_held("restart") == Some(cut)));
    let port = h.crash();
    let h = Harness::start(Options {
        port,
        ..Options::default()
    });
    let target = Target::Lease("vm-a".into());
    for (role, credential) in [(Role::Agent, &a.agent), (Role::Client, &a.client)] {
        h.broker.admit(
            &target,
            role,
            &relay_broker::credential_sha256(credential),
            None,
        );
    }
    assert!(h.broker.wait_for_agent(&target, WAIT).is_some());
    assert!(client.wait_end(WAIT), "the old client session is gone");
    let client = client_v2(&url, &a.client);
    let streamed = client.stream(1, "restart", write("r.bin"), &whole[..], true, None);
    assert_eq!(streamed, Streamed::Sent { from: cut });
    let result = client.wait(1, WAIT).result.unwrap();
    assert!(matches!(result.outcome, Outcome::Written(_)));
    assert_eq!(std::fs::read(home.join("r.bin")).unwrap(), whole);
}

/// RP-15 (TL-2): an upload the broker reads itself continues on the
/// agent's next session from its committed offset, without the caller.
#[test]
fn an_in_process_upload_continues_on_the_agents_next_session() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.bin");
    let whole = pattern(4 * UPLOAD_WINDOW as usize, 5);
    std::fs::write(&source, &whole).unwrap();
    let broker = h.broker.clone();
    let target = a.target.clone();
    let cutter = std::thread::spawn(move || {
        assert!(wait_until(WAIT, || agent
            .agent
            .upload_held("local")
            .is_some_and(|held| held > UPLOAD_WINDOW)));
        assert!(broker.end_agent_session(&target));
        agent
    });
    let answer = h
        .broker
        .run(
            &a.target,
            "local",
            write("l.bin"),
            Upload::file(&source).unwrap(),
            WAIT * 4,
        )
        .unwrap();
    let agent = cutter.join().unwrap();
    assert!(matches!(answer.outcome, Outcome::Written(d) if d.size == whole.len() as u64));
    assert_eq!(std::fs::read(agent.home.join("l.bin")).unwrap(), whole);
    assert!(
        h.logged("sending it again"),
        "the upload went to a new session"
    );
}

/// RP-15 (TL-2): a short stream never commits and waits to be continued; a
/// corrupted one never commits and leaves no record, so its ID may be sent
/// again.
#[test]
fn a_short_or_corrupted_stream_never_commits_through_the_broker() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = client_v2(&h.url("ws"), &a.client);
    let whole = pattern(2 * MAX_CHUNK + 10, 6);

    client.stream(1, "short", write("s.bin"), &whole[..], false, Some(1000));
    client.send(&relay_agent::protocol::Message::End {
        call: 1,
        execution: "short".into(),
        size: whole.len() as u64,
        sha256: guest_actions::sha256_hex(&whole),
    });
    let result = client.wait(1, WAIT).result.unwrap();
    assert_eq!(code(&result.outcome), Some(("upload_incomplete", false)));
    assert!(!agent.home.join("s.bin").exists());
    client.query(2, "short");
    assert_eq!(client.wait(2, WAIT).query, Some(ExecutionState::Unknown));

    let mut corrupted = whole.clone();
    corrupted[MAX_CHUNK + 5] ^= 0xff;
    client.stream(
        3,
        "corrupt",
        write("c.bin"),
        &corrupted[..],
        false,
        Some(corrupted.len() as u64),
    );
    client.send(&relay_agent::protocol::Message::End {
        call: 3,
        execution: "corrupt".into(),
        size: whole.len() as u64,
        sha256: guest_actions::sha256_hex(&whole),
    });
    let result = client.wait(3, WAIT).result.unwrap();
    assert_eq!(code(&result.outcome), Some(("checksum_mismatch", false)));
    assert!(!agent.home.join("c.bin").exists());
    assert_eq!(agent.agent.upload_held("corrupt"), None);
    client.query(4, "corrupt");
    assert_eq!(client.wait(4, WAIT).query, Some(ExecutionState::Unknown));
    // The same ID, sent again with the right bytes, writes once.
    client.stream(5, "corrupt", write("c.bin"), &whole[..], false, None);
    assert!(matches!(
        client.wait(5, WAIT).result.unwrap().outcome,
        Outcome::Written(_)
    ));
    assert_eq!(std::fs::read(agent.home.join("c.bin")).unwrap(), whole);
}

/// RP-15 (TL-2): a client that sends past the window is refused before it
/// can fill the broker, even while no agent takes its bytes.
#[test]
fn a_client_that_ignores_the_window_is_refused() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let client = client_v2(&h.url("ws"), &a.client);
    client.send(&relay_agent::protocol::Message::Call(
        relay_agent::protocol::Call {
            call: 1,
            execution: "greedy".into(),
            op: write("g.bin"),
            lease: None,
            resume: false,
            offset: 0,
        },
    ));
    let piece = vec![7u8; MAX_CHUNK];
    for _ in 0..(UPLOAD_WINDOW as usize / MAX_CHUNK + 4) {
        client.send(&relay_agent::protocol::Message::Chunk {
            call: 1,
            data: piece.clone(),
        });
    }
    let result = client.wait(1, WAIT).result.unwrap();
    assert_eq!(code(&result.outcome), Some(("window_exceeded", false)));
    assert!(h.broker.held_most() <= UPLOAD_WINDOW + 2 * MAX_CHUNK as u64);
}

/// RP-15 (TL-2): a read starts at the offset the client names, and its end
/// names the whole file, so a client continues a read it lost.
#[test]
fn a_read_starts_at_the_clients_offset() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = client_v2(&h.url("ws"), &a.client);
    let whole = pattern(UPLOAD_WINDOW as usize + 2 * MAX_CHUNK + 1, 7);
    std::fs::write(agent.home.join("big.bin"), &whole).unwrap();
    let offset = MAX_CHUNK as u64 + 33;
    client.send(&relay_agent::protocol::Message::Call(
        relay_agent::protocol::Call {
            call: 1,
            execution: "read-from".into(),
            op: Op::Read {
                path: "big.bin".into(),
            },
            lease: None,
            resume: false,
            offset,
        },
    ));
    let answer = client.wait(1, WAIT);
    assert_eq!(answer.bytes, &whole[offset as usize..]);
    let end = answer.end.unwrap();
    assert_eq!(end.size, whole.len() as u64);
    assert_eq!(end.sha256, guest_actions::sha256_hex(&whole));
}

/// RP-13, RP-15 (TL-2): a tree of 10,000 files moves each way through the
/// broker, as one tar stream.
#[test]
fn a_tree_of_ten_thousand_files_moves_each_way_through_the_broker() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = client_v2(&h.url("ws"), &a.client);
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    for folder in 0..100 {
        let folder = source.join(format!("d{folder:03}"));
        std::fs::create_dir_all(&folder).unwrap();
        for file in 0..100 {
            std::fs::write(folder.join(format!("f{file:03}")), format!("{file}")).unwrap();
        }
    }
    let stream = byte_transfer::pack_into(&source, Vec::new()).unwrap();
    let op = Op::WriteTree {
        path: "tree".into(),
        name: String::new(),
    };
    client.stream(1, "tree-up", op, &stream[..], false, None);
    let result = client.wait(1, WAIT).result.unwrap();
    assert!(matches!(result.outcome, Outcome::Written(d) if d.size == stream.len() as u64));
    let count: usize = std::fs::read_dir(agent.home.join("tree"))
        .unwrap()
        .map(|folder| std::fs::read_dir(folder.unwrap().path()).unwrap().count())
        .sum();
    assert_eq!(count, 10_000);
    client.call(
        2,
        "tree-down",
        Op::ReadTree {
            path: "tree".into(),
        },
        b"",
    );
    let answer = client.wait(2, WAIT);
    assert!(answer.end_matches());
    let archive = dir.path().join("back.tar");
    std::fs::write(&archive, &answer.bytes).unwrap();
    byte_transfer::unpack(&archive, &dir.path().join("back")).unwrap();
    let count: usize = std::fs::read_dir(dir.path().join("back"))
        .unwrap()
        .map(|folder| std::fs::read_dir(folder.unwrap().path()).unwrap().count())
        .sum();
    assert_eq!(count, 10_000);
}
