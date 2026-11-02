//! TL-1: execution records without sessions.

use std::time::Duration;

use relay_agent::protocol::{CallResult, Digest, ExecutionState, Op, OpKind, Outcome};

use crate::credentials::Target;
use crate::job::{Job, JobEvent};

fn exec_job() -> Job {
    Job::new(
        Target::Lease("vm".into()),
        "e".into(),
        Op::Exec {
            command: "true".into(),
            stdin: false,
            timeout_s: 1.0,
        },
        Vec::new(),
        false,
        8,
    )
}

fn exit() -> Outcome {
    Outcome::Exit {
        rc: Some(0),
        signal: None,
        timed_out: false,
        cancelled: false,
        output: Digest {
            size: 6,
            sha256: "x".into(),
        },
    }
}

fn result(replayed: bool, outcome: Outcome) -> CallResult {
    CallResult {
        call: 1,
        execution: "e".into(),
        replayed,
        outcome,
    }
}

fn drain(rx: &std::sync::mpsc::Receiver<JobEvent>) -> Vec<JobEvent> {
    rx.try_iter().collect()
}

#[test]
fn a_resent_calls_replay_is_not_delivered_twice() {
    let job = exec_job();
    let origin = job.subscribe(true).unwrap();
    let (_, _) = job.hand_on_for_test();
    let mut seen = 0;
    job.on_chunk_for_test(&mut seen, b"abc");
    // The session ends; the next session's replay starts from byte 0.
    let (before, _) = job.hand_on_for_test();
    assert!(before);
    let mut seen = 0;
    job.on_chunk_for_test(&mut seen, b"ab");
    job.on_chunk_for_test(&mut seen, b"cdef");
    let forget = job.on_result_for_test(result(true, exit()), before);
    assert!(!forget);
    let events = drain(&origin);
    assert_eq!(
        events,
        vec![
            JobEvent::Chunk(b"abc".to_vec()),
            JobEvent::Chunk(b"def".to_vec()),
            JobEvent::Done {
                replayed: false,
                outcome: exit()
            },
        ]
    );
    assert_eq!(
        job.state(),
        ExecutionState::Completed {
            op: OpKind::Exec,
            outcome: exit()
        }
    );
}

#[test]
fn late_callers_get_the_kept_output_and_a_replay_flag() {
    let job = exec_job();
    let _origin = job.subscribe(true).unwrap();
    job.hand_on_for_test();
    let mut seen = 0;
    job.on_chunk_for_test(&mut seen, b"abcd");
    let late = job.subscribe(false).unwrap();
    job.on_chunk_for_test(&mut seen, b"ef");
    job.on_end_for_test(Digest {
        size: 6,
        sha256: "x".into(),
    });
    job.on_result_for_test(result(false, exit()), false);
    let events = drain(&late);
    assert_eq!(events[0], JobEvent::Chunk(b"abcd".to_vec()));
    assert_eq!(events[1], JobEvent::Chunk(b"ef".to_vec()));
    assert!(matches!(events[2], JobEvent::End(_)));
    assert!(matches!(events[3], JobEvent::Done { replayed: true, .. }));
    // Past the bound of 8 kept bytes, a late caller needs the agent's replay.
    let mut seen = 6;
    job.on_chunk_for_test(&mut seen, b"ghijk");
    assert!(job.subscribe(false).is_none());
}

#[test]
fn a_refusal_forgets_the_record_and_an_agent_replay_is_reported() {
    let job = exec_job();
    let origin = job.subscribe(true).unwrap();
    job.hand_on_for_test();
    assert!(job.on_result_for_test(
        result(false, Outcome::refused("checksum_mismatch", "no")),
        false
    ));
    assert!(matches!(
        drain(&origin)[0],
        JobEvent::Done {
            replayed: false,
            ..
        }
    ));

    // The agent had the ID before this broker did: a replay for the origin.
    let job = exec_job();
    let origin = job.subscribe(true).unwrap();
    job.hand_on_for_test();
    assert!(!job.on_result_for_test(result(true, exit()), false));
    assert!(matches!(
        drain(&origin)[0],
        JobEvent::Done { replayed: true, .. }
    ));
    assert!(!job.expired(std::time::Instant::now(), Duration::from_secs(3600)));
    assert!(job.expired(
        std::time::Instant::now() + Duration::from_secs(3601),
        Duration::from_secs(3600)
    ));
}
