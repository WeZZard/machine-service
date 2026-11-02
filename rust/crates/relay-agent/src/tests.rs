//! TL-1 unit tests (`docs/arch/test-layers.md`): the relay agent on threads,
//! reached over in-memory links. No network, broker or guest is involved.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::agent::{Agent, AgentOptions};
use crate::config::{self, ConfigError};
use crate::dial::{Backoff, BACKOFF_MAX, BACKOFF_MIN};
use crate::driver::{Answer, Driver};
use crate::link::{memory_pair, Link, LinkError, MemoryLink, StreamLink};
use crate::protocol::*;

const LONG: f64 = 30.0;

fn options(home: &Path) -> AgentOptions {
    AgentOptions {
        shell: PathBuf::from("/bin/sh"),
        home: home.to_path_buf(),
        state: home.join(".state"),
        build: "test-build".into(),
        heartbeat: Duration::from_secs(30),
        retention: Duration::from_secs(3600),
        ownership: guest_actions::ownership::Ownership::detect(),
        executable: std::env::current_exe().unwrap(),
    }
}

type Serving = JoinHandle<Result<(), LinkError>>;

/// Serve a new in-memory link and return the broker's end of it.
fn connect(agent: &Agent) -> (Driver, Serving) {
    let (broker, end) = memory_pair();
    let agent = agent.clone();
    let serving = thread::spawn(move || agent.serve(Arc::new(end)));
    (Driver::open(Arc::new(broker)).unwrap(), serving)
}

struct Rig {
    home: tempfile::TempDir,
    agent: Agent,
    driver: Driver,
    serving: Serving,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(|_| {})
    }

    fn with(adjust: impl FnOnce(&mut AgentOptions)) -> Rig {
        let home = tempfile::tempdir().unwrap();
        let mut options = options(home.path());
        adjust(&mut options);
        let agent = Agent::open(options).unwrap();
        let (driver, serving) = connect(&agent);
        Rig {
            home,
            agent,
            driver,
            serving,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn run(&mut self, execution: &str, op: Op, upload: &[u8]) -> Answer {
        self.driver.run(execution, op, upload).unwrap()
    }

    fn exec(&mut self, execution: &str, command: &str) -> Answer {
        self.run(execution, exec(command), b"")
    }

    /// Close the link and serve a new one.
    fn reconnect(&mut self) {
        self.driver.link().close();
        let (driver, serving) = connect(&self.agent);
        let old = std::mem::replace(&mut self.serving, serving);
        old.join().unwrap().unwrap();
        self.driver = driver;
    }

    /// Stop the agent, open its records again and connect.
    fn restart(&mut self) {
        self.agent.shutdown(Duration::from_secs(5));
        let agent = Agent::open(self.agent.options().clone()).unwrap();
        let (driver, serving) = connect(&agent);
        let _ = std::mem::replace(&mut self.serving, serving).join();
        self.agent = agent;
        self.driver = driver;
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.agent.shutdown(Duration::from_secs(5));
    }
}

fn exec(command: &str) -> Op {
    Op::Exec {
        command: command.into(),
        stdin: false,
        timeout_s: LONG,
    }
}

fn outcome(answer: &Answer) -> &Outcome {
    &answer.result.as_ref().expect("a result").outcome
}

fn exit_code(answer: &Answer) -> Option<i32> {
    match outcome(answer) {
        Outcome::Exit { rc, .. } => *rc,
        other => panic!("not an exit: {other:?}"),
    }
}

fn text(answer: &Answer) -> String {
    String::from_utf8_lossy(&answer.bytes).into_owned()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Pump the driver until `call` has received a line of output.
fn first_line(driver: &mut Driver, call: u64) -> String {
    loop {
        if let Some(answer) = driver.received(call) {
            let output = String::from_utf8_lossy(&answer.bytes).into_owned();
            if let Some((line, _)) = output.split_once('\n') {
                return line.to_string();
            }
        }
        driver.pump().unwrap();
    }
}

fn wait_settled(driver: &mut Driver, execution: &str) -> ExecutionState {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = driver.query(execution).unwrap();
        if !matches!(state, ExecutionState::Running { .. }) {
            return state;
        }
        assert!(Instant::now() < deadline, "{execution} never settled");
        thread::sleep(Duration::from_millis(20));
    }
}

fn leftovers(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect()
}

// ------------------------------------------------------------------ protocol

#[test]
fn every_message_survives_encoding() {
    let digest = Digest {
        size: 3,
        sha256: "ab".into(),
    };
    let messages = vec![
        Message::Hello(Hello {
            protocol: PROTOCOL,
            build: "b".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            heartbeat_ms: 10_000,
            in_flight: vec![Report {
                execution: "x".into(),
                state: ExecutionState::Running { op: OpKind::Exec },
            }],
        }),
        Message::Heartbeat,
        Message::Call(Call {
            call: 1,
            execution: "e".into(),
            op: exec("true"),
        }),
        Message::Call(Call {
            call: 2,
            execution: "e".into(),
            op: Op::Script { timeout_s: 1.5 },
        }),
        Message::Call(Call {
            call: 3,
            execution: "e".into(),
            op: Op::Write {
                path: "p".into(),
                name: "n".into(),
                mode: Some(0o644),
            },
        }),
        Message::Call(Call {
            call: 4,
            execution: "e".into(),
            op: Op::Read { path: "p".into() },
        }),
        Message::Call(Call {
            call: 5,
            execution: "e".into(),
            op: Op::Cancel { target: "t".into() },
        }),
        Message::Call(Call {
            call: 6,
            execution: "e".into(),
            op: Op::Status,
        }),
        Message::Call(Call {
            call: 8,
            execution: "e".into(),
            op: Op::WriteTree {
                path: "p".into(),
                name: "n".into(),
            },
        }),
        Message::Call(Call {
            call: 9,
            execution: "e".into(),
            op: Op::ReadTree { path: "p".into() },
        }),
        Message::Chunk {
            call: u64::MAX,
            data: vec![0, 1, 2, 255],
        },
        Message::Chunk {
            call: 7,
            data: Vec::new(),
        },
        Message::End {
            call: 7,
            execution: "e".into(),
            size: 3,
            sha256: "ab".into(),
        },
        Message::Result(CallResult {
            call: 8,
            execution: "e".into(),
            replayed: true,
            outcome: Outcome::Exit {
                rc: Some(0),
                signal: None,
                timed_out: false,
                cancelled: false,
                output: digest.clone(),
            },
        }),
        Message::Query {
            call: 9,
            execution: "e".into(),
        },
        Message::QueryResult {
            call: 9,
            execution: "e".into(),
            state: ExecutionState::Completed {
                op: OpKind::Write,
                outcome: Outcome::Written(digest),
            },
        },
        Message::QueryResult {
            call: 10,
            execution: "e".into(),
            state: ExecutionState::Unknown,
        },
    ];
    for message in messages {
        let decoded = Message::decode(&message.encode()).unwrap();
        assert_eq!(decoded, message);
    }
    let outcomes = vec![
        Outcome::Read(Digest {
            size: 0,
            sha256: String::new(),
        }),
        Outcome::Cancel {
            target: "t".into(),
            found: CancelFound::Discarded,
        },
        Outcome::Status(Status {
            protocol: PROTOCOL,
            build: "b".into(),
            os: "linux".into(),
            running: vec!["a".into()],
            ownership: "scope".into(),
        }),
        Outcome::refused("checksum_mismatch", "m"),
        Outcome::Interrupted {
            message: "m".into(),
        },
    ];
    for outcome in outcomes {
        let message = Message::Result(CallResult {
            call: 1,
            execution: "e".into(),
            replayed: false,
            outcome,
        });
        assert_eq!(Message::decode(&message.encode()).unwrap(), message);
    }
}

#[test]
fn a_full_chunk_fits_in_one_message_and_a_larger_one_is_refused() {
    let full = Message::Chunk {
        call: 1,
        data: vec![7; MAX_CHUNK],
    }
    .encode();
    assert!(full.len() <= MAX_MESSAGE);
    let mut over = full.clone();
    over.push(0);
    assert!(matches!(
        Message::decode(&over),
        Err(DecodeError::LargeChunk(_))
    ));
    assert!(matches!(
        Message::decode(&[1, 0, 0]),
        Err(DecodeError::ShortChunk(3))
    ));
    assert!(matches!(
        Message::decode(b"{\"nope\":1}"),
        Err(DecodeError::Json(_))
    ));
}

// ---------------------------------------------------------------------- links

#[test]
fn a_memory_link_is_ordered_refuses_large_messages_and_closes_both_ends() {
    let (a, b) = memory_pair();
    for index in 0..100u8 {
        a.send(vec![index]).unwrap();
    }
    for index in 0..100u8 {
        assert_eq!(b.receive().unwrap(), vec![index]);
    }
    assert_eq!(
        a.send(vec![0; MAX_MESSAGE + 1]),
        Err(LinkError::TooLarge(MAX_MESSAGE + 1))
    );
    a.send(vec![1]).unwrap();
    a.close();
    assert_eq!(b.receive().unwrap(), vec![1]);
    assert_eq!(b.receive(), Err(LinkError::Closed));
    assert_eq!(b.send(vec![2]), Err(LinkError::Closed));
    assert_eq!(a.receive(), Err(LinkError::Closed));
}

#[test]
fn closing_a_memory_link_wakes_its_own_receiver() {
    let (a, _b) = memory_pair();
    let a = Arc::new(a);
    let waiting = {
        let a = Arc::clone(&a);
        thread::spawn(move || a.receive())
    };
    thread::sleep(Duration::from_millis(50));
    a.close();
    assert_eq!(waiting.join().unwrap(), Err(LinkError::Closed));
}

#[test]
fn a_stream_link_frames_messages() {
    let (reader, writer) = std::io::pipe().unwrap();
    let link = StreamLink::new(reader, writer);
    link.send(b"one".to_vec()).unwrap();
    link.send(Vec::new()).unwrap();
    assert_eq!(link.receive().unwrap(), b"one");
    assert_eq!(link.receive().unwrap(), b"");
    assert!(matches!(
        link.send(vec![0; MAX_MESSAGE + 1]),
        Err(LinkError::TooLarge(_))
    ));
    link.close();
    assert_eq!(link.receive(), Err(LinkError::Closed));
}

// ---------------------------------------------------------------- hello, beat

#[test]
fn the_hello_names_protocol_build_platform_and_heartbeat() {
    let rig = Rig::new();
    let hello = rig.driver.hello();
    assert_eq!(hello.protocol, PROTOCOL);
    assert_eq!(hello.build, "test-build");
    assert_eq!(hello.os, std::env::consts::OS);
    assert_eq!(hello.arch, std::env::consts::ARCH);
    assert_eq!(hello.heartbeat_ms, 30_000);
    assert!(hello.in_flight.is_empty());
}

#[test]
fn the_default_heartbeat_keeps_within_thirty_seconds() {
    assert!(crate::agent::HEARTBEAT <= Duration::from_secs(30));
}

#[test]
fn heartbeats_arrive_at_the_announced_interval() {
    let mut rig = Rig::with(|options| options.heartbeat = Duration::from_millis(50));
    let started = Instant::now();
    while rig.driver.heartbeats < 4 {
        rig.driver.pump().unwrap();
    }
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ----------------------------------------------------------------------- exec

#[test]
fn exec_streams_combined_output_that_ends_with_its_sha256() {
    let mut rig = Rig::new();
    let answer = rig.exec("e1", "echo out; echo err >&2; exit 3");
    assert_eq!(text(&answer), "out\nerr\n");
    assert!(answer.end_matches());
    assert_eq!(exit_code(&answer), Some(3));
    let Outcome::Exit { output, .. } = outcome(&answer) else {
        unreachable!()
    };
    assert_eq!(Some(output), answer.end.as_ref());
    assert!(!answer.result.as_ref().unwrap().replayed);
}

#[test]
fn exec_runs_through_the_named_shell_in_the_home_directory() {
    let mut rig = Rig::new();
    let answer = rig.exec("e1", "echo $0; pwd");
    let output = text(&answer);
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("sh"));
    assert_eq!(
        std::fs::canonicalize(lines.next().unwrap()).unwrap(),
        std::fs::canonicalize(rig.home.path()).unwrap()
    );
}

#[test]
fn a_missing_command_exits_127() {
    let mut rig = Rig::new();
    let answer = rig.exec("e1", "no-such-command-anywhere");
    assert_eq!(exit_code(&answer), Some(127));
}

#[test]
fn exec_feeds_the_upload_to_standard_input_only_when_asked() {
    let mut rig = Rig::new();
    let with = Op::Exec {
        command: "cat".into(),
        stdin: true,
        timeout_s: LONG,
    };
    assert_eq!(rig.run("e1", with, b"piped bytes").bytes, b"piped bytes");
    assert_eq!(rig.run("e2", exec("cat"), b"ignored").bytes, b"");
}

#[test]
fn a_script_runs_on_the_standard_input_of_the_platform_shell() {
    let mut rig = Rig::new();
    let script = b"echo $0\nprintf 'line %s\\n' 1 2\nexit 4\n";
    let answer = rig.run("s1", Op::Script { timeout_s: LONG }, script);
    let expected = if cfg!(target_os = "macos") {
        "zsh"
    } else {
        "bash"
    };
    assert_eq!(text(&answer), format!("{expected}\nline 1\nline 2\n"));
    assert_eq!(exit_code(&answer), Some(4));
}

#[test]
fn large_output_arrives_in_chunks_of_at_most_1_mib() {
    let mut rig = Rig::new();
    let answer = rig.exec("big", "head -c 3500000 /dev/zero");
    assert_eq!(answer.bytes.len(), 3_500_000);
    assert!(answer.end_matches());
    assert!(answer.chunks.iter().all(|size| *size <= MAX_CHUNK));
}

#[test]
fn a_timeout_kills_the_whole_process_group() {
    let mut rig = Rig::new();
    let op = Op::Exec {
        command: "sleep 30 & echo $!; wait".into(),
        stdin: false,
        timeout_s: 0.5,
    };
    let call = rig.driver.start("t1", op, b"").unwrap();
    let pid: i32 = first_line(&mut rig.driver, call).parse().unwrap();
    let answer = rig.driver.finish(call).unwrap();
    let Outcome::Exit {
        timed_out, signal, ..
    } = outcome(&answer)
    else {
        panic!("{answer:?}")
    };
    assert!(timed_out);
    assert_eq!(*signal, Some(libc::SIGKILL));
    wait_gone(pid);
}

#[test]
fn an_upload_that_does_not_match_its_end_never_runs_and_is_not_recorded() {
    let mut rig = Rig::new();
    let call = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "m1".into(),
            op: exec("touch ran"),
        }))
        .unwrap();
    rig.driver
        .send(&Message::Chunk {
            call,
            data: b"abc".to_vec(),
        })
        .unwrap();
    rig.driver
        .send(&Message::End {
            call,
            execution: "m1".into(),
            size: 3,
            sha256: guest_actions::sha256_hex(b"abd"),
        })
        .unwrap();
    let answer = rig.driver.finish(call).unwrap();
    assert_eq!(
        outcome(&answer),
        &Outcome::refused("checksum_mismatch", "the upload does not match its end")
    );
    assert_eq!(rig.driver.query("m1").unwrap(), ExecutionState::Unknown);
    assert!(!rig.path("ran").exists());
    // Nothing ran, so the same execution ID may be sent again.
    assert_eq!(exit_code(&rig.exec("m1", "touch ran")), Some(0));
    assert!(rig.path("ran").exists());
}

#[test]
fn an_exec_without_its_end_never_runs() {
    let mut rig = Rig::new();
    let call = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "n1".into(),
            op: exec("touch ran"),
        }))
        .unwrap();
    rig.reconnect();
    thread::sleep(Duration::from_millis(100));
    assert!(!rig.path("ran").exists());
    assert_eq!(rig.driver.query("n1").unwrap(), ExecutionState::Unknown);
}

#[test]
fn a_call_resent_before_its_end_supersedes_the_first() {
    let mut rig = Rig::new();
    let first = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call: first,
            execution: "r1".into(),
            op: exec("echo run >> runs"),
        }))
        .unwrap();
    let second = rig
        .driver
        .start("r1", exec("echo run >> runs"), b"")
        .unwrap();
    let superseded = rig.driver.finish(first).unwrap();
    assert!(matches!(
        outcome(&superseded),
        Outcome::Failed { code, committed: false, .. } if code == "superseded"
    ));
    assert_eq!(exit_code(&rig.driver.finish(second).unwrap()), Some(0));
    // The first call's late end finds nothing to commit.
    rig.driver
        .send(&Message::End {
            call: first,
            execution: "r1".into(),
            size: 0,
            sha256: guest_actions::sha256_hex(b""),
        })
        .unwrap();
    rig.exec("sync", "true");
    assert_eq!(std::fs::read_to_string(rig.path("runs")).unwrap(), "run\n");
}

#[test]
fn an_end_for_another_execution_never_commits_a_pending_call() {
    let mut rig = Rig::new();
    let call = rig.driver.next_call();
    let write = |execution: &str| {
        Message::Call(Call {
            call,
            execution: execution.into(),
            op: Op::Write {
                path: format!("{execution}.txt"),
                name: String::new(),
                mode: None,
            },
        })
    };
    rig.driver.send(&write("a")).unwrap();
    // A broker that reuses the call number: refused, but its chunks and end
    // still arrive under that number.
    rig.driver.send(&write("b")).unwrap();
    rig.driver
        .send(&Message::Chunk {
            call,
            data: b"bee".to_vec(),
        })
        .unwrap();
    rig.driver
        .send(&Message::End {
            call,
            execution: "b".into(),
            size: 3,
            sha256: guest_actions::sha256_hex(b"bee"),
        })
        .unwrap();
    rig.exec("sync", "true");
    assert!(!rig.path("a.txt").exists() && !rig.path("b.txt").exists());
    assert_eq!(rig.driver.query("a").unwrap(), ExecutionState::Unknown);
    assert_eq!(rig.driver.query("b").unwrap(), ExecutionState::Unknown);
    assert!(leftovers(rig.home.path()).is_empty());
}

#[test]
fn bad_calls_are_refused_without_a_record() {
    let mut rig = Rig::new();
    let answer = rig.exec("", "true");
    assert!(
        matches!(outcome(&answer), Outcome::Failed { code, committed: false, .. } if code == "invalid_request")
    );
    let long = "x".repeat(crate::agent::MAX_EXECUTION_ID + 1);
    assert!(matches!(
        outcome(&rig.exec(&long, "true")),
        Outcome::Failed {
            committed: false,
            ..
        }
    ));
    let zero = Op::Exec {
        command: "true".into(),
        stdin: false,
        timeout_s: 0.0,
    };
    assert!(
        matches!(outcome(&rig.run("z", zero, b"")), Outcome::Failed { code, .. } if code == "invalid_request")
    );
    assert_eq!(rig.driver.query("z").unwrap(), ExecutionState::Unknown);
}

#[test]
fn a_call_number_in_use_is_refused() {
    let mut rig = Rig::new();
    let call = rig.driver.next_call();
    let send = |driver: &Driver, execution: &str| {
        driver
            .send(&Message::Call(Call {
                call,
                execution: execution.into(),
                op: exec("true"),
            }))
            .unwrap();
    };
    send(&rig.driver, "a");
    send(&rig.driver, "b");
    let refused = rig.driver.finish(call).unwrap();
    assert!(matches!(outcome(&refused), Outcome::Failed { code, .. } if code == "call_in_use"));
    assert_eq!(refused.result.unwrap().execution, "b");
}

#[test]
fn a_malformed_message_closes_the_link() {
    let home = tempfile::tempdir().unwrap();
    let agent = Agent::open(options(home.path())).unwrap();
    let (driver, serving) = connect(&agent);
    driver.link().send(b"not json".to_vec()).unwrap();
    assert!(matches!(serving.join().unwrap(), Err(LinkError::Failed(_))));
}

// --------------------------------------------------------------------- cancel

#[test]
fn cancel_kills_a_running_command_group() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start("c1", exec("sleep 30 & echo $!; wait"), b"")
        .unwrap();
    let pid: i32 = first_line(&mut rig.driver, call).parse().unwrap();
    let cancel = rig
        .driver
        .run(
            "c2",
            Op::Cancel {
                target: "c1".into(),
            },
            b"",
        )
        .unwrap();
    assert_eq!(
        outcome(&cancel),
        &Outcome::Cancel {
            target: "c1".into(),
            found: CancelFound::Signalled
        }
    );
    let answer = rig.driver.finish(call).unwrap();
    assert!(matches!(
        outcome(&answer),
        Outcome::Exit {
            cancelled: true,
            signal: Some(9),
            ..
        }
    ));
    wait_gone(pid);
}

#[test]
fn cancel_reports_completed_unknown_and_discarded_targets() {
    let mut rig = Rig::new();
    rig.exec("done", "true");
    let found = |rig: &mut Rig, id: &str, target: &str| match outcome(&rig.run(
        id,
        Op::Cancel {
            target: target.into(),
        },
        b"",
    )) {
        Outcome::Cancel { found, .. } => *found,
        other => panic!("{other:?}"),
    };
    assert_eq!(found(&mut rig, "k1", "done"), CancelFound::Completed);
    assert_eq!(found(&mut rig, "k2", "nobody"), CancelFound::Unknown);
    let call = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "waiting".into(),
            op: exec("touch ran"),
        }))
        .unwrap();
    assert_eq!(found(&mut rig, "k3", "waiting"), CancelFound::Discarded);
    let discarded = rig.driver.finish(call).unwrap();
    assert!(
        matches!(outcome(&discarded), Outcome::Failed { code, committed: false, .. } if code == "cancelled")
    );
    rig.driver
        .send(&Message::End {
            call,
            execution: "waiting".into(),
            size: 0,
            sha256: guest_actions::sha256_hex(b""),
        })
        .unwrap();
    rig.exec("sync", "true");
    assert!(!rig.path("ran").exists());
}

// --------------------------------------------------------------- write, read

#[test]
fn a_file_written_and_read_back_matches_with_its_mode() {
    let mut rig = Rig::new();
    let bytes: Vec<u8> = (0..3_200_000u32).map(|n| (n * 31 % 251) as u8).collect();
    let write = Op::Write {
        path: "~/data.bin".into(),
        name: String::new(),
        mode: Some(0o640),
    };
    let written = rig.run("w1", write, &bytes);
    let digest = Digest {
        size: bytes.len() as u64,
        sha256: guest_actions::sha256_hex(&bytes),
    };
    assert_eq!(outcome(&written), &Outcome::Written(digest.clone()));
    let mode = std::fs::metadata(rig.path("data.bin"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o640);
    let read = rig.run(
        "r1",
        Op::Read {
            path: "data.bin".into(),
        },
        b"",
    );
    assert_eq!(read.bytes, bytes);
    assert!(read.end_matches());
    assert!(read.chunks.len() >= 4 && read.chunks.iter().all(|size| *size <= MAX_CHUNK));
    assert_eq!(outcome(&read), &Outcome::Read(digest));
    assert!(leftovers(rig.home.path()).is_empty());
}

#[test]
fn a_write_into_a_directory_uses_the_given_name_and_replaces_whole() {
    let mut rig = Rig::new();
    std::fs::create_dir(rig.path("dir")).unwrap();
    std::fs::write(rig.path("dir/f.txt"), b"an older and longer content").unwrap();
    let write = Op::Write {
        path: "dir".into(),
        name: "f.txt".into(),
        mode: None,
    };
    assert!(matches!(
        outcome(&rig.run("w1", write, b"new")),
        Outcome::Written(_)
    ));
    assert_eq!(std::fs::read(rig.path("dir/f.txt")).unwrap(), b"new");
}

#[test]
fn a_write_that_fails_its_check_or_loses_its_link_leaves_nothing() {
    let mut rig = Rig::new();
    let call = rig.driver.next_call();
    let op = Op::Write {
        path: "f".into(),
        name: String::new(),
        mode: None,
    };
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "w1".into(),
            op: op.clone(),
        }))
        .unwrap();
    rig.driver
        .send(&Message::Chunk {
            call,
            data: b"abc".to_vec(),
        })
        .unwrap();
    rig.driver
        .send(&Message::End {
            call,
            execution: "w1".into(),
            size: 4,
            sha256: guest_actions::sha256_hex(b"abc"),
        })
        .unwrap();
    let answer = rig.driver.finish(call).unwrap();
    assert!(
        matches!(outcome(&answer), Outcome::Failed { code, committed: false, .. } if code == "checksum_mismatch")
    );
    let call = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "w2".into(),
            op,
        }))
        .unwrap();
    rig.driver
        .send(&Message::Chunk {
            call,
            data: b"abc".to_vec(),
        })
        .unwrap();
    rig.exec("sync", "true");
    assert_eq!(leftovers(rig.home.path()).len(), 1);
    rig.reconnect();
    assert!(!rig.path("f").exists());
    assert!(leftovers(rig.home.path()).is_empty());
}

#[test]
fn a_write_to_a_missing_directory_is_refused() {
    let mut rig = Rig::new();
    let write = Op::Write {
        path: "no/such/dir/f".into(),
        name: String::new(),
        mode: None,
    };
    let answer = rig.run("w1", write, b"x");
    assert!(
        matches!(outcome(&answer), Outcome::Failed { code, committed: false, .. } if code == "not_found")
    );
}

#[test]
fn reads_of_a_directory_or_a_missing_file_fail() {
    let mut rig = Rig::new();
    let read =
        |rig: &mut Rig, id: &str, path: &str| rig.run(id, Op::Read { path: path.into() }, b"");
    let directory = read(&mut rig, "r1", "~");
    assert!(
        matches!(outcome(&directory), Outcome::Failed { code, committed: true, .. } if code == "is_directory")
    );
    let missing = read(&mut rig, "r2", "nope");
    assert!(matches!(outcome(&missing), Outcome::Failed { code, .. } if code == "not_found"));
    assert!(missing.end.is_none());
}

// --------------------------------------------------------------------- status

#[test]
fn status_reports_the_running_executions() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start("slow", exec("echo go; sleep 30"), b"")
        .unwrap();
    first_line(&mut rig.driver, call);
    let status = rig.run("st1", Op::Status, b"");
    let Outcome::Status(status) = outcome(&status) else {
        panic!()
    };
    assert_eq!(status.protocol, PROTOCOL);
    assert_eq!(status.build, "test-build");
    assert_eq!(status.os, std::env::consts::OS);
    assert_eq!(status.running, vec!["slow".to_string()]);
    rig.run(
        "k",
        Op::Cancel {
            target: "slow".into(),
        },
        b"",
    );
    rig.driver.finish(call).unwrap();
}

// -------------------------------------------------------------- at most once

#[test]
fn a_repeated_execution_id_returns_the_recorded_outcome_and_never_runs_again() {
    let mut rig = Rig::new();
    let first = rig.exec("once", "echo run >> runs; echo output; exit 5");
    let again = rig.exec("once", "echo run >> runs; echo output; exit 5");
    assert!(again.result.as_ref().unwrap().replayed);
    assert_eq!(again.bytes, first.bytes);
    assert_eq!(outcome(&again), outcome(&first));
    // Even a different operation under the same ID returns the record.
    let other = rig.run("once", Op::Status, b"");
    assert_eq!(outcome(&other), outcome(&first));
    assert_eq!(std::fs::read_to_string(rig.path("runs")).unwrap(), "run\n");
}

#[test]
fn a_repeated_id_attaches_to_the_running_command_on_a_new_link() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start(
            "live",
            exec("echo start; sleep 0.5; echo end; echo run >> runs"),
            b"",
        )
        .unwrap();
    assert_eq!(first_line(&mut rig.driver, call), "start");
    rig.reconnect();
    assert_eq!(
        rig.driver.query("live").unwrap(),
        ExecutionState::Running { op: OpKind::Exec }
    );
    let attached = rig.exec("live", "echo run >> runs");
    assert_eq!(text(&attached), "start\nend\n");
    assert!(attached.end_matches());
    assert!(attached.result.as_ref().unwrap().replayed);
    assert_eq!(exit_code(&attached), Some(0));
    assert_eq!(std::fs::read_to_string(rig.path("runs")).unwrap(), "run\n");
}

#[test]
fn query_answers_completed_running_and_unknown() {
    let mut rig = Rig::new();
    let done = rig.exec("q1", "true");
    assert_eq!(
        rig.driver.query("q1").unwrap(),
        ExecutionState::Completed {
            op: OpKind::Exec,
            outcome: outcome(&done).clone()
        }
    );
    let call = rig
        .driver
        .start("q2", exec("echo go; sleep 30"), b"")
        .unwrap();
    first_line(&mut rig.driver, call);
    assert_eq!(
        rig.driver.query("q2").unwrap(),
        ExecutionState::Running { op: OpKind::Exec }
    );
    assert_eq!(rig.driver.query("q3").unwrap(), ExecutionState::Unknown);
    rig.run(
        "k",
        Op::Cancel {
            target: "q2".into(),
        },
        b"",
    );
    rig.driver.finish(call).unwrap();
}

#[test]
fn records_survive_a_restart_of_the_agent() {
    let mut rig = Rig::new();
    let first = rig.exec("durable", "echo run >> runs; echo hi");
    let write = Op::Write {
        path: "f".into(),
        name: String::new(),
        mode: None,
    };
    let written = rig.run("durable-w", write.clone(), b"one");
    rig.restart();
    let again = rig.exec("durable", "echo run >> runs; echo hi");
    assert!(again.result.as_ref().unwrap().replayed);
    assert_eq!(text(&again), "hi\n");
    assert_eq!(outcome(&again), outcome(&first));
    std::fs::write(rig.path("f"), b"changed").unwrap();
    let rewritten = rig.run("durable-w", write, b"two");
    assert_eq!(outcome(&rewritten), outcome(&written));
    assert_eq!(std::fs::read(rig.path("f")).unwrap(), b"changed");
    assert_eq!(std::fs::read_to_string(rig.path("runs")).unwrap(), "run\n");
}

#[test]
fn a_command_running_at_a_restart_is_killed_interrupted_and_never_run_again() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start(
            "long",
            exec("sleep 30 & echo $!; wait; echo run >> runs"),
            b"",
        )
        .unwrap();
    let pid: i32 = first_line(&mut rig.driver, call).parse().unwrap();
    rig.restart();
    wait_gone(pid);
    let state = rig.driver.query("long").unwrap();
    assert!(matches!(
        state,
        ExecutionState::Completed {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    ));
    let again = rig.exec("long", "echo run >> runs");
    assert!(matches!(outcome(&again), Outcome::Interrupted { .. }));
    assert!(!rig.path("runs").exists());
}

#[test]
fn a_record_left_running_by_a_crash_opens_as_interrupted() {
    let home = tempfile::tempdir().unwrap();
    let options = options(home.path());
    {
        let mut store = crate::store::Store::open(&options.state, options.retention).unwrap();
        store.begin("crashed", OpKind::Exec).unwrap();
    }
    let agent = Agent::open(options).unwrap();
    assert!(matches!(
        agent.query("crashed"),
        ExecutionState::Completed {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    ));
}

#[test]
fn an_expired_record_is_dropped() {
    let mut rig = Rig::with(|options| options.retention = Duration::from_millis(200));
    rig.exec("old", "true");
    thread::sleep(Duration::from_millis(300));
    rig.reconnect();
    assert_eq!(rig.driver.query("old").unwrap(), ExecutionState::Unknown);
}

#[test]
fn any_execution_id_is_a_safe_record_name() {
    let mut rig = Rig::new();
    let id = "../../escape/\u{0}/..";
    assert_eq!(exit_code(&rig.exec(id, "true")), Some(0));
    rig.restart();
    assert!(matches!(
        rig.driver.query(id).unwrap(),
        ExecutionState::Completed { .. }
    ));
    assert!(!rig.home.path().join("escape").exists());
}

#[test]
fn the_hello_reports_what_ran_while_no_link_carried_it() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start("away", exec("echo go; sleep 0.4"), b"")
        .unwrap();
    first_line(&mut rig.driver, call);
    rig.reconnect();
    let reports = &rig.driver.hello().in_flight;
    assert_eq!(
        reports,
        &vec![Report {
            execution: "away".into(),
            state: ExecutionState::Running { op: OpKind::Exec }
        }]
    );
    // It completes with no link to carry its result.
    rig.driver.link().close();
    thread::sleep(Duration::from_millis(700));
    let (driver, serving) = connect(&rig.agent);
    rig.driver = driver;
    let _ = std::mem::replace(&mut rig.serving, serving).join();
    let reports = &rig.driver.hello().in_flight;
    assert_eq!(reports.len(), 1);
    assert!(matches!(
        &reports[0].state,
        ExecutionState::Completed {
            outcome: Outcome::Exit { rc: Some(0), .. },
            ..
        }
    ));
    // A hello that carried the outcome reported it.
    rig.reconnect();
    assert!(rig.driver.hello().in_flight.is_empty());
}

// --------------------------------------------------------- randomized faults

/// xorshift64*, so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

#[derive(Clone)]
struct Planned {
    execution: String,
    op: Op,
    upload: Vec<u8>,
    /// The file the operation writes or appends to.
    effect: String,
}

fn plan(rng: &mut Rng, index: usize) -> Planned {
    let execution = format!("op-{index}");
    let marker = format!("runs/{execution}");
    match rng.below(5) {
        0 | 1 => {
            let command = match rng.below(3) {
                0 => format!("echo run >> {marker}"),
                1 => format!("echo run >> {marker}; sleep 0.2"),
                _ => format!("sleep 0.2; echo run >> {marker}"),
            };
            let stdin = rng.chance(50);
            let command = if stdin {
                format!("cat > /dev/null; {command}")
            } else {
                command
            };
            let upload = (0..rng.below(3000)).map(|n| n as u8).collect();
            Planned {
                execution,
                op: Op::Exec {
                    command,
                    stdin,
                    timeout_s: LONG,
                },
                upload,
                effect: marker,
            }
        }
        2 => Planned {
            execution,
            op: Op::Script { timeout_s: LONG },
            upload: format!("echo run >> {marker}\necho done\n").into_bytes(),
            effect: marker,
        },
        _ => {
            let size = if rng.chance(10) {
                2_500_000
            } else {
                rng.below(5000)
            };
            let seed = rng.next();
            let upload = (0..size).map(|n| (n ^ seed) as u8).collect();
            let effect = format!("files/{execution}");
            Planned {
                execution,
                op: Op::Write {
                    path: effect.clone(),
                    name: String::new(),
                    mode: None,
                },
                upload,
                effect,
            }
        }
    }
}

/// The messages of one call, with its upload split at random sizes.
fn messages(rng: &mut Rng, call: u64, planned: &Planned) -> Vec<Message> {
    let mut messages = vec![Message::Call(Call {
        call,
        execution: planned.execution.clone(),
        op: planned.op.clone(),
    })];
    let mut rest = &planned.upload[..];
    while !rest.is_empty() {
        let size = (1 + rng.below(MAX_CHUNK as u64) as usize).min(rest.len());
        messages.push(Message::Chunk {
            call,
            data: rest[..size].to_vec(),
        });
        rest = &rest[size..];
    }
    messages.push(Message::End {
        call,
        execution: planned.execution.clone(),
        size: planned.upload.len() as u64,
        sha256: guest_actions::sha256_hex(&planned.upload),
    });
    messages
}

/// The broker's end of a link under faults: every message the agent sends
/// is collected by a thread.
struct Faulty {
    link: Arc<MemoryLink>,
    serving: Serving,
    collector: JoinHandle<()>,
}

fn faulty(agent: &Agent, inbox: &Arc<Mutex<Vec<Message>>>) -> Faulty {
    let (broker, end) = memory_pair();
    let link = Arc::new(broker);
    let agent = agent.clone();
    let serving = thread::spawn(move || agent.serve(Arc::new(end)));
    let collector = {
        let link = Arc::clone(&link);
        let inbox = Arc::clone(inbox);
        thread::spawn(move || {
            while let Ok(payload) = link.receive() {
                inbox
                    .lock()
                    .unwrap()
                    .push(Message::decode(&payload).unwrap());
            }
        })
    };
    Faulty {
        link,
        serving,
        collector,
    }
}

impl Faulty {
    fn cut(self) {
        self.link.close();
        let _ = self.serving.join();
        let _ = self.collector.join();
    }
}

/// One seeded run; returns the number of link cuts and agent restarts.
fn chaos(seed: u64) -> usize {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join("runs")).unwrap();
    std::fs::create_dir(home.path().join("files")).unwrap();
    let options = options(home.path());
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let planned: Vec<Planned> = (0..16).map(|index| plan(&mut rng, index)).collect();
    let inbox: Arc<Mutex<Vec<Message>>> = Arc::default();
    let mut agent = Agent::open(options.clone()).unwrap();
    let mut link = faulty(&agent, &inbox);
    let mut held: Vec<Message> = Vec::new();
    let mut next_call = 1u64;
    let (mut cuts, mut restarts) = (0, 0);
    for _ in 0..60 {
        match rng.below(100) {
            0..=6 => {
                link.cut();
                cuts += 1;
                link = faulty(&agent, &inbox);
            }
            7..=10 => {
                link.cut();
                agent.shutdown(Duration::from_secs(5));
                restarts += 1;
                agent = Agent::open(options.clone()).unwrap();
                link = faulty(&agent, &inbox);
            }
            11..=20 => {
                // Delayed messages arrive late, in any order, maybe on a
                // later link than the one they were sent for.
                while !held.is_empty() {
                    let message = held.swap_remove(rng.below(held.len() as u64) as usize);
                    let _ = link.link.send(message.encode());
                }
            }
            21..=25 => {
                let execution = planned[rng.below(planned.len() as u64) as usize]
                    .execution
                    .clone();
                let _ = link
                    .link
                    .send(Message::Query { call: 0, execution }.encode());
            }
            _ => {
                let chosen = &planned[rng.below(planned.len() as u64) as usize];
                // A resend may reuse an earlier call number.
                let call = if rng.chance(15) && next_call > 1 {
                    1 + rng.below(next_call - 1)
                } else {
                    next_call += 1;
                    next_call - 1
                };
                for message in messages(&mut rng, call, chosen) {
                    match rng.below(100) {
                        0..=5 => {}
                        6..=11 => {
                            let _ = link.link.send(message.encode());
                            let _ = link.link.send(message.encode());
                        }
                        12..=19 => held.push(message),
                        _ => {
                            let _ = link.link.send(message.encode());
                        }
                    }
                }
            }
        }
        if rng.chance(30) {
            thread::sleep(Duration::from_millis(rng.below(30)));
        }
    }
    link.cut();

    // Settle on a clean link and check every execution ID.
    let (mut driver, serving) = connect(&agent);
    let marker_count = |planned: &Planned| -> usize {
        std::fs::read_to_string(home.path().join(&planned.effect))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let mut finals: HashMap<String, ExecutionState> = HashMap::new();
    for planned in &planned {
        let state = wait_settled(&mut driver, &planned.execution);
        let context = format!("seed {seed}, {}: {state:?}", planned.execution);
        let is_write = matches!(planned.op, Op::Write { .. });
        if !is_write {
            assert!(marker_count(planned) <= 1, "ran twice: {context}");
        }
        match &state {
            ExecutionState::Unknown => {
                assert!(
                    !home.path().join(&planned.effect).exists(),
                    "ran without a record: {context}"
                );
            }
            ExecutionState::Completed { outcome, .. } => match outcome {
                Outcome::Exit {
                    rc: Some(0),
                    cancelled: false,
                    ..
                } => assert_eq!(marker_count(planned), 1, "{context}"),
                Outcome::Written(digest) => {
                    assert_eq!(digest.sha256, guest_actions::sha256_hex(&planned.upload));
                    let bytes = std::fs::read(home.path().join(&planned.effect)).unwrap();
                    assert_eq!(bytes, planned.upload, "{context}");
                }
                Outcome::Interrupted { .. } => {}
                other => panic!("unexpected outcome {other:?}: {context}"),
            },
            ExecutionState::Running { .. } => unreachable!(),
        }
        finals.insert(planned.execution.clone(), state);
    }
    // Every committed result the broker saw is the recorded outcome.
    for message in inbox.lock().unwrap().iter() {
        if let Message::Result(result) = message {
            if matches!(
                result.outcome,
                Outcome::Failed {
                    committed: false,
                    ..
                }
            ) {
                continue;
            }
            let Some(ExecutionState::Completed { outcome, .. }) = finals.get(&result.execution)
            else {
                panic!(
                    "seed {seed}: a result for {} with no record",
                    result.execution
                );
            };
            assert_eq!(
                &result.outcome, outcome,
                "seed {seed}: {}",
                result.execution
            );
        }
    }
    // A clean resend of everything runs only what never committed.
    for planned in &planned {
        let answer = driver
            .run(&planned.execution, planned.op.clone(), &planned.upload)
            .unwrap();
        let result = answer.result.unwrap();
        let committed_before = !matches!(finals[&planned.execution], ExecutionState::Unknown);
        assert_eq!(
            result.replayed, committed_before,
            "seed {seed}: {}",
            planned.execution
        );
    }
    for planned in &planned {
        wait_settled(&mut driver, &planned.execution);
        if !matches!(planned.op, Op::Write { .. }) {
            assert!(
                marker_count(planned) <= 1,
                "seed {seed}: {} ran twice",
                planned.execution
            );
        }
    }
    assert!(
        leftovers(&home.path().join("files")).is_empty(),
        "seed {seed}"
    );
    driver.link().close();
    let _ = serving.join();
    agent.shutdown(Duration::from_secs(5));
    cuts + restarts
}

#[test]
fn randomized_faults_never_run_an_execution_id_twice() {
    let faults: usize = (1..=12).map(chaos).sum();
    assert!(
        faults >= 12,
        "the seeds cut or restarted only {faults} times"
    );
}

// --------------------------------------------------------------------- config

fn write_config(path: &Path, content: &str, mode: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(path);
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

const GOOD: &str =
    r#"{"broker_url": "wss://broker.example:6241/relay", "agent_credential": "token"}"#;

#[test]
fn the_configuration_lives_under_the_home_directory() {
    assert_eq!(
        config::config_path(Path::new("/h")),
        Path::new("/h/.config/relay-agent/config.json")
    );
}

#[test]
fn a_private_configuration_loads() {
    let home = tempfile::tempdir().unwrap();
    let path = config::config_path(home.path());
    write_config(&path, GOOD, 0o600);
    let config = config::load(&path).unwrap();
    assert_eq!(config.broker_url, "wss://broker.example:6241/relay");
    assert_eq!(config.agent_credential, "token");
    assert!(!format!("{config:?}").contains("token"));
}

#[test]
fn a_configuration_others_can_read_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let path = config::config_path(home.path());
    for mode in [0o644, 0o640, 0o400, 0o700] {
        write_config(&path, GOOD, mode);
        assert_eq!(
            config::load(&path),
            Err(ConfigError::Mode {
                path: path.clone(),
                mode
            })
        );
    }
}

#[test]
fn a_missing_or_invalid_configuration_is_reported() {
    let home = tempfile::tempdir().unwrap();
    let path = config::config_path(home.path());
    assert_eq!(config::load(&path), Err(ConfigError::Missing(path.clone())));
    for content in [
        "not json",
        r#"{"broker_url": "https://x", "agent_credential": "t"}"#,
        r#"{"broker_url": "wss://x", "agent_credential": ""}"#,
        r#"{"broker_url": "wss://x"}"#,
    ] {
        write_config(&path, content, 0o600);
        assert!(
            matches!(config::load(&path), Err(ConfigError::Invalid { .. })),
            "{content}"
        );
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(config::load(&path), Err(ConfigError::NotAFile(_))));
}

#[test]
fn waiting_for_the_configuration_reports_each_reason_once() {
    let home = tempfile::tempdir().unwrap();
    let path = config::config_path(home.path());
    let writer = {
        let path = path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            write_config(&path, GOOD, 0o644);
            thread::sleep(Duration::from_millis(100));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        })
    };
    let mut reasons = Vec::new();
    let config = config::wait_for(&path, Duration::from_millis(10), |reason| {
        reasons.push(reason.clone())
    });
    writer.join().unwrap();
    assert_eq!(config.agent_credential, "token");
    assert_eq!(
        reasons,
        vec![
            ConfigError::Missing(path.clone()),
            ConfigError::Mode {
                path: path.clone(),
                mode: 0o644
            }
        ]
    );
}

// ----------------------------------------------------------------------- dial

#[test]
fn backoff_grows_from_250_ms_to_5_s_with_jitter() {
    let mut backoff = Backoff::seeded(7);
    let delays: Vec<Duration> = (0..10).map(|_| backoff.next_delay()).collect();
    assert!(delays[0] >= BACKOFF_MIN / 2 && delays[0] <= BACKOFF_MIN);
    assert!(delays.iter().all(|delay| *delay <= BACKOFF_MAX));
    assert!(delays[9] >= BACKOFF_MAX / 2);
    let mut distinct = delays.clone();
    distinct.dedup();
    assert!(distinct.len() > 5, "{delays:?}");
    backoff.reset();
    assert!(backoff.next_delay() <= BACKOFF_MIN);
}

#[test]
fn run_serves_each_dialed_link_and_dials_again_after_failures() {
    let home = tempfile::tempdir().unwrap();
    let agent = Agent::open(options(home.path())).unwrap();
    let config = config::Config {
        broker_url: "wss://broker.example/relay".into(),
        agent_credential: "t".into(),
        broker_ca_pem: None,
    };
    let (brokers, received) = std::sync::mpsc::channel::<MemoryLink>();
    let running = {
        let agent = agent.clone();
        thread::spawn(move || {
            let mut attempts = 0;
            let mut reasons = Vec::new();
            crate::dial::run(
                &agent,
                &config,
                |_| {
                    attempts += 1;
                    if attempts % 2 == 1 {
                        return Err(LinkError::Failed("refused".into()));
                    }
                    let (broker, end) = memory_pair();
                    brokers.send(broker).unwrap();
                    Ok(end)
                },
                |reason| reasons.push(reason.clone()),
            );
            reasons
        })
    };
    for round in 0..2 {
        let broker: Arc<dyn Link> = Arc::new(received.recv().unwrap());
        let mut driver = Driver::open(broker).unwrap();
        let answer = driver
            .run(&format!("run-{round}"), exec("echo hi"), b"")
            .unwrap();
        assert_eq!(answer.bytes, b"hi\n");
        driver.link().close();
    }
    agent.shutdown(Duration::from_secs(1));
    let reasons = running.join().unwrap();
    assert!(reasons.contains(&LinkError::Failed("refused".into())));
}

/// DC-139 (TL-1): an agent that opens the records of one that stopped
/// while commands ran stops the group whose leader still has its recorded
/// start time, leaves alone a process that reused a recorded ID, and marks
/// both executions interrupted.
#[test]
fn dc139_a_restarted_agent_stops_only_groups_whose_start_time_matches() {
    use guest_actions::ownership::{process_start, Owner};
    use std::os::unix::process::CommandExt;
    let home = tempfile::tempdir().unwrap();
    let spawn = || {
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap()
    };
    let mut ours = spawn();
    let mut unrelated = spawn();
    let (ours_pid, unrelated_pid) = (ours.id() as i32, unrelated.id() as i32);
    {
        let mut store =
            crate::store::Store::open(&home.path().join(".state"), Duration::from_secs(3600))
                .unwrap();
        store.begin("left-running", OpKind::Exec).unwrap();
        store
            .set_owner(
                "left-running",
                Owner::Group {
                    group: ours_pid,
                    started: process_start(ours_pid).unwrap(),
                },
            )
            .unwrap();
        store.begin("id-reused", OpKind::Exec).unwrap();
        store
            .set_owner(
                "id-reused",
                Owner::Group {
                    group: unrelated_pid,
                    started: "0.000000".into(),
                },
            )
            .unwrap();
    }
    let agent = Agent::open(options(home.path())).unwrap();
    let status = ours.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL),
        "the command's own group is stopped"
    );
    assert!(alive(unrelated_pid), "an unrelated process is left alone");
    for (execution, says) in [("left-running", "was stopped"), ("id-reused", "left alone")] {
        match agent.query(execution) {
            ExecutionState::Completed {
                outcome: Outcome::Interrupted { message },
                ..
            } => assert!(message.contains(says), "{execution}: {message}"),
            other => panic!("{execution}: {other:?}"),
        }
    }
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

#[test]
fn dc139_a_command_records_its_owner_and_status_names_the_ownership() {
    let mut rig = Rig::new();
    let call = rig
        .driver
        .start("owned", exec("echo $$; sleep 30"), b"")
        .unwrap();
    let group: i32 = first_line(&mut rig.driver, call).parse().unwrap();
    let status = rig.run("owned-status", Op::Status, b"");
    let expected = guest_actions::ownership::Ownership::detect().describe();
    assert!(matches!(outcome(&status), Outcome::Status(s) if s.ownership == expected));
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            rig.path(".state/executions")
                .join(crate::store::Store::key("owned"))
                .join("record.json"),
        )
        .unwrap(),
    )
    .unwrap();
    if expected == "scope" {
        let unit = record["owner"]["scope"]["unit"].as_str().unwrap();
        let prefix = format!("relay-agent-{}-", crate::store::Store::key("owned"));
        assert!(unit.starts_with(&prefix), "{unit}");
        // The command runs in that scope.
        let cgroup = std::fs::read_to_string(format!("/proc/{group}/cgroup")).unwrap();
        assert!(cgroup.contains(unit), "{cgroup}");
    } else {
        assert_eq!(record["owner"]["group"]["group"], group);
        assert_eq!(
            record["owner"]["group"]["started"].as_str(),
            guest_actions::ownership::process_start(group).as_deref()
        );
    }
    rig.run(
        "owned-cancel",
        Op::Cancel {
            target: "owned".into(),
        },
        b"",
    );
    rig.driver.finish(call).unwrap();
}

// ---------------------------------------------------------------- folders

/// A folder's tar stream, as a client packs it.
fn folder_stream(source: &Path) -> Vec<u8> {
    byte_transfer::pack_into(source, Vec::new()).unwrap()
}

/// RP-13 (TL-1): a folder written as a tar stream lands whole at its end,
/// reads back as the same files, and its execution IDs replay like a
/// write's and a read's.
#[test]
fn a_folder_written_and_read_back_matches_and_replays() {
    let mut rig = Rig::new();
    let source = rig.path("source");
    std::fs::create_dir_all(source.join("nested/deeper")).unwrap();
    let big: Vec<u8> = (0..2_500_000u32).map(|n| (n * 7 % 251) as u8).collect();
    std::fs::write(source.join("nested/deeper/big.bin"), &big).unwrap();
    std::fs::write(source.join("a.txt"), "alpha").unwrap();
    std::fs::set_permissions(source.join("a.txt"), std::fs::Permissions::from_mode(0o750)).unwrap();
    let stream = folder_stream(&source);
    std::fs::create_dir(rig.path("into")).unwrap();
    let write = Op::WriteTree {
        path: "into".into(),
        name: "copy".into(),
    };
    let written = rig.run("t-w", write.clone(), &stream);
    let digest = Digest {
        size: stream.len() as u64,
        sha256: guest_actions::sha256_hex(&stream),
    };
    assert_eq!(outcome(&written), &Outcome::Written(digest.clone()));
    assert_eq!(
        std::fs::read(rig.path("into/copy/nested/deeper/big.bin")).unwrap(),
        big
    );
    let mode = std::fs::metadata(rig.path("into/copy/a.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o750);

    // The same ID never writes again, even after the folder changed.
    std::fs::write(rig.path("into/copy/a.txt"), "changed").unwrap();
    let again = rig.run("t-w", write, &stream);
    assert!(again.result.as_ref().unwrap().replayed);
    assert_eq!(
        std::fs::read_to_string(rig.path("into/copy/a.txt")).unwrap(),
        "changed"
    );

    let read = rig.run(
        "t-r",
        Op::ReadTree {
            path: "into/copy".into(),
        },
        b"",
    );
    assert!(read.end_matches());
    assert!(read.chunks.len() >= 3 && read.chunks.iter().all(|size| *size <= MAX_CHUNK));
    assert!(matches!(outcome(&read), Outcome::Read(d) if d.size == read.bytes.len() as u64));
    let archive = rig.path("back.tar");
    std::fs::write(&archive, &read.bytes).unwrap();
    byte_transfer::unpack(&archive, &rig.path("back")).unwrap();
    assert_eq!(
        std::fs::read(rig.path("back/nested/deeper/big.bin")).unwrap(),
        big
    );
    assert_eq!(
        std::fs::read_to_string(rig.path("back/a.txt")).unwrap(),
        "changed"
    );
    // A repeated read ID answers its end and result without the bytes.
    let replayed = rig.run(
        "t-r",
        Op::ReadTree {
            path: "into/copy".into(),
        },
        b"",
    );
    assert!(replayed.result.as_ref().unwrap().replayed);
    assert!(replayed.bytes.is_empty());
    assert_eq!(outcome(&replayed), outcome(&read));
    assert!(leftovers(&rig.path("into")).is_empty());
}

/// RP-13 (TL-1): a folder upload that does not match its end, or that is
/// not a tar stream of files and folders, never lands and is not recorded;
/// a folder read of a file is refused.
#[test]
fn a_bad_folder_upload_never_lands_and_is_not_recorded() {
    let mut rig = Rig::new();
    let source = rig.path("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("f"), "x").unwrap();
    let stream = folder_stream(&source);
    let call = rig.driver.next_call();
    rig.driver
        .send(&Message::Call(Call {
            call,
            execution: "bad".into(),
            op: Op::WriteTree {
                path: "dest".into(),
                name: String::new(),
            },
        }))
        .unwrap();
    rig.driver
        .send(&Message::Chunk {
            call,
            data: stream.clone(),
        })
        .unwrap();
    rig.driver
        .send(&Message::End {
            call,
            execution: "bad".into(),
            size: stream.len() as u64,
            sha256: guest_actions::sha256_hex(b"other"),
        })
        .unwrap();
    let refused = rig.driver.finish(call).unwrap();
    assert!(matches!(
        outcome(&refused),
        Outcome::Failed { code, committed: false, .. } if code == "checksum_mismatch"
    ));
    assert_eq!(rig.agent.query("bad"), ExecutionState::Unknown);
    let garbage = vec![7u8; 4096];
    let refused = rig.run(
        "garbage",
        Op::WriteTree {
            path: "dest".into(),
            name: String::new(),
        },
        &garbage,
    );
    assert!(
        matches!(
            outcome(&refused),
            Outcome::Failed { code, committed: false, .. } if code == "bad_archive"
        ),
        "{:?}",
        outcome(&refused)
    );
    assert!(!rig.path("dest").exists());
    let names: Vec<String> = std::fs::read_dir(rig.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with(".state"))
        .collect();
    assert_eq!(names, ["source"], "no spool or staged folder is left");
    let read = rig.run(
        "file-as-tree",
        Op::ReadTree {
            path: "source/f".into(),
        },
        b"",
    );
    assert!(matches!(outcome(&read), Outcome::Failed { code, .. } if code == "not_a_directory"));
}
