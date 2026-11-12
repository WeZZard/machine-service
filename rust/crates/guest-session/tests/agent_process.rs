//! TL-2 component integration (`docs/arch/test-layers.md`): the real
//! `guest-session-agent` binary as a child process, reached through a test
//! link that can cut the stream or flip a byte in it, standing in for a
//! channel (`ssh -T`, `tart exec -i`). Covers AC-3, AC-4 and AC-5 at this
//! layer.

use std::io::{self, PipeWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use guest_session::agent::hex;
use guest_session::client::{Op, Session, SessionError};
use sha2::{Digest, Sha256};

const AGENT: &str = env!("CARGO_BIN_EXE_guest-session-agent");
const OPEN: Duration = Duration::from_secs(10);
const LONG: Duration = Duration::from_secs(120);

/// What the link does to the agent's output.
#[derive(Clone, Copy, Default)]
struct Faults {
    /// Flip one bit of the agent's output at this byte offset.
    corrupt_output_at: Option<u64>,
    /// Flip one bit of the host's input to the agent at this byte offset.
    corrupt_input_at: Option<u64>,
    /// Cut the link once this many bytes of agent output have passed.
    cut_output_after: Option<u64>,
}

/// A guest session agent behind a link the test controls.
struct Link {
    child: service_layout::test_build::harness::Owned,
    to_agent: Arc<Mutex<Option<ChildStdin>>>,
    to_host: Arc<Mutex<Option<PipeWriter>>>,
}

impl Link {
    fn start(home: &Path, faults: Faults) -> (Link, Session) {
        let mut child = service_layout::test_build::harness::command(AGENT)
            .arg("serve")
            .env("HOME", home)
            .env("SHELL", "/bin/sh")
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the agent");
        let (host_reader, link_writer) = io::pipe().unwrap();
        let (mut link_reader, host_writer) = io::pipe().unwrap();
        let to_agent = Arc::new(Mutex::new(child.child().stdin.take()));
        let to_host = Arc::new(Mutex::new(Some(link_writer)));
        {
            // Host → agent. On a failed write the link stops reading, so the
            // host sees its own writes fail instead of blocking.
            let to_agent = Arc::clone(&to_agent);
            thread::spawn(move || {
                pump(
                    &mut link_reader,
                    faults.corrupt_input_at,
                    None,
                    |bytes| match to_agent.lock().unwrap().as_mut() {
                        Some(stdin) => stdin.write_all(bytes).is_ok(),
                        None => false,
                    },
                );
            });
        }
        {
            // Agent → host.
            let mut agent_output = child.child().stdout.take().unwrap();
            let to_agent = Arc::clone(&to_agent);
            let to_host = Arc::clone(&to_host);
            thread::spawn(move || {
                pump(
                    &mut agent_output,
                    faults.corrupt_output_at,
                    faults.cut_output_after,
                    |bytes| match to_host.lock().unwrap().as_mut() {
                        Some(writer) => writer.write_all(bytes).is_ok(),
                        None => false,
                    },
                );
                // A cut closes both directions, as a dropped connection does.
                to_agent.lock().unwrap().take();
                to_host.lock().unwrap().take();
            });
        }
        let session = Session::open(host_reader, host_writer, OPEN).expect("open a session");
        (
            Link {
                child,
                to_agent,
                to_host,
            },
            session,
        )
    }

    /// Drop the connection in both directions, as a broken SSH connection
    /// does: the agent's input ends and the host's input ends.
    fn cut(&self) {
        self.to_agent.lock().unwrap().take();
        self.to_host.lock().unwrap().take();
    }

    /// Kill the agent itself, as a guest-side crash does.
    fn crash(&mut self) {
        let _ = self.child.child().kill();
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.cut();
        self.child.end();
    }
}

/// Copy `source` through `deliver` until either ends, flipping one bit at
/// `corrupt_at` and stopping after `cut_after` bytes.
fn pump(
    source: &mut dyn Read,
    corrupt_at: Option<u64>,
    cut_after: Option<u64>,
    mut deliver: impl FnMut(&[u8]) -> bool,
) {
    let mut offset = 0u64;
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        let mut count = match source.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        if let Some(at) = corrupt_at {
            if (offset..offset + count as u64).contains(&at) {
                buffer[(at - offset) as usize] ^= 0x01;
            }
        }
        if let Some(limit) = cut_after {
            if offset + count as u64 >= limit {
                count = (limit - offset) as usize;
                deliver(&buffer[..count]);
                return;
            }
        }
        offset += count as u64;
        if !deliver(&buffer[..count]) {
            return;
        }
    }
}

fn random_file(path: &Path, size: usize) -> String {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut bytes = vec![0u8; size];
    for chunk in bytes.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    std::fs::write(path, &bytes).unwrap();
    hex(&Sha256::digest(&bytes))
}

fn sha256_of(path: &Path) -> String {
    hex(&Sha256::digest(std::fs::read(path).unwrap()))
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for_line(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if content.ends_with('\n') {
                return content;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn temporary_files(directory: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".tmp"))
        .collect()
}

#[test]
fn the_hello_names_the_agents_own_build() {
    let home = tempfile::tempdir().unwrap();
    let (_link, session) = Link::start(home.path(), Faults::default());
    assert_eq!(session.hello().build, sha256_of(Path::new(AGENT)));
    assert_eq!(session.hello().os, std::env::consts::OS);
    assert_eq!(
        session.hello().heartbeat,
        Some(guest_session::agent::HEARTBEAT)
    );
    assert_eq!(
        session.silence_limit(),
        Some(guest_session::agent::HEARTBEAT * guest_session::client::MISSED_HEARTBEATS)
    );
}

#[test]
fn many_operations_share_one_session() {
    let home = tempfile::tempdir().unwrap();
    let (_link, mut session) = Link::start(home.path(), Faults::default());
    let started = Instant::now();
    for index in 0..100 {
        let outcome = session.exec(&format!("echo {index}"), None, LONG).unwrap();
        assert_eq!(outcome.output, format!("{index}\n").into_bytes());
    }
    let elapsed = started.elapsed();
    let file = home.path().join("small");
    std::fs::write(&file, b"small").unwrap();
    for _ in 0..20 {
        session.write_from_path(&file, "copy", LONG).unwrap();
        let mut back = Vec::new();
        session.read("copy", &mut back, LONG).unwrap();
        assert_eq!(back, b"small");
    }
    eprintln!("100 execs over one session: {elapsed:?}");
}

/// AC-3: 64 MiB each way arrives with the same SHA-256.
#[test]
fn sixty_four_mebibytes_round_trip_intact() {
    let home = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let source = host.path().join("source.bin");
    let digest = random_file(&source, 64 << 20);
    let (_link, mut session) = Link::start(home.path(), Faults::default());
    let started = Instant::now();
    let written = session.write_from_path(&source, "guest.bin", LONG).unwrap();
    let write_time = started.elapsed();
    assert_eq!(
        (written.size, written.sha256.as_str()),
        (64 << 20, digest.as_str())
    );
    assert_eq!(sha256_of(&home.path().join("guest.bin")), digest);
    let started = Instant::now();
    let back = host.path().join("back.bin");
    let read = session.read_to_path("guest.bin", &back, LONG).unwrap();
    let read_time = started.elapsed();
    assert_eq!(read.sha256, digest);
    assert_eq!(sha256_of(&back), digest);
    eprintln!("64 MiB write {write_time:?}, read {read_time:?}");
}

/// AC-3: a byte changed on the way to the host fails the read, and nothing
/// appears at the destination.
#[test]
fn a_byte_corrupted_toward_the_host_fails_the_read_and_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    random_file(&home.path().join("guest.bin"), 4 << 20);
    let (_link, mut session) = Link::start(
        home.path(),
        Faults {
            corrupt_output_at: Some(1 << 20),
            ..Faults::default()
        },
    );
    let destination = host.path().join("copy.bin");
    let error = session
        .read_to_path("guest.bin", &destination, LONG)
        .unwrap_err();
    assert!(matches!(error, SessionError::Lost { .. }), "{error}");
    assert!(error.to_string().contains("checksum"), "{error}");
    assert!(error.may_resend(Op::Read));
    assert!(!destination.exists());
    assert!(temporary_files(host.path()).is_empty());
}

/// AC-3: a byte changed on the way to the guest stops the agent before the
/// file is renamed into place, and the write never reached its commit point.
#[test]
fn a_byte_corrupted_toward_the_guest_fails_the_write_and_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let source = host.path().join("source.bin");
    random_file(&source, 32 << 20);
    std::fs::write(home.path().join("guest.bin"), b"before").unwrap();
    let (_link, mut session) = Link::start(
        home.path(),
        Faults {
            corrupt_input_at: Some(2 << 20),
            ..Faults::default()
        },
    );
    let error = session
        .write_from_path(&source, "guest.bin", LONG)
        .unwrap_err();
    assert!(
        matches!(
            error,
            SessionError::Lost {
                committed: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.may_resend(Op::Write));
    assert_eq!(
        std::fs::read(home.path().join("guest.bin")).unwrap(),
        b"before"
    );
    // The agent removes its unfinished file as it stops.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !temporary_files(home.path()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "temporary file left in the guest"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// AC-4: an exec whose connection is lost is uncertain, ran once, and its
/// command is stopped in the guest; the next session works.
#[test]
fn an_exec_cut_midway_is_uncertain_ran_once_and_was_stopped() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("marker");
    let pid_file = home.path().join("pid");
    let (link, mut session) = Link::start(home.path(), Faults::default());
    let command = format!(
        "echo run >> {}; echo $$ > {}; exec sleep 30",
        marker.display(),
        pid_file.display()
    );
    let cutter = {
        let pid_file = pid_file.clone();
        thread::spawn(move || {
            wait_for_line(&pid_file);
            link
        })
    };
    let waiter = thread::spawn(move || session.exec(&command, None, LONG));
    let link = cutter.join().unwrap();
    link.cut();
    let error = waiter.join().unwrap().unwrap_err();
    assert!(error.is_uncertain(Op::Exec), "{error}");
    assert!(!error.may_resend(Op::Exec));
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the command kept running after the cut"
        );
        thread::sleep(Duration::from_millis(20));
    }
    drop(link);
    let (_link, mut next) = Link::start(home.path(), Faults::default());
    let count = next.exec("wc -l < marker", None, LONG).unwrap();
    assert_eq!(String::from_utf8_lossy(&count.output).trim(), "1");
}

/// AC-4 with the agent itself dying: still uncertain, never resent.
#[test]
fn an_exec_whose_agent_crashes_is_uncertain() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let (mut link, mut session) = Link::start(home.path(), Faults::default());
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    let waiter = thread::spawn(move || session.exec(&command, None, LONG));
    let pid: i32 = wait_for_line(&pid_file).trim().parse().unwrap();
    link.crash();
    let error = waiter.join().unwrap().unwrap_err();
    assert!(error.is_uncertain(Op::Exec), "{error}");
    // A crashed agent cannot stop its commands; the test cleans up.
    // SAFETY: plain signal delivery to a process this test started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// AC-5: a read cut midway is resent on a new session and succeeds.
#[test]
fn a_read_cut_midway_succeeds_on_a_new_session() {
    let home = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let digest = random_file(&home.path().join("guest.bin"), 16 << 20);
    let destination = host.path().join("copy.bin");
    let (_link, mut session) = Link::start(
        home.path(),
        Faults {
            cut_output_after: Some(5 << 20),
            ..Faults::default()
        },
    );
    let error = session
        .read_to_path("guest.bin", &destination, LONG)
        .unwrap_err();
    assert!(
        error.may_resend(Op::Read) && !error.is_uncertain(Op::Read),
        "{error}"
    );
    assert!(!destination.exists());
    let (_link, mut next) = Link::start(home.path(), Faults::default());
    assert_eq!(
        next.read_to_path("guest.bin", &destination, LONG)
            .unwrap()
            .sha256,
        digest
    );
    assert_eq!(sha256_of(&destination), digest);
}
