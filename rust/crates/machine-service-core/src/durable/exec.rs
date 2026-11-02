//! Running one command of a durable lease (`docs/arch/durable-machine.md`,
//! DM-4).
//!
//! The command is a child of the service, so it runs as the service's user in
//! the service's GUI session and macOS attributes its privacy permissions to
//! the service (DC-29). It starts in a new process session (`setsid`), which
//! the service records before it waits, so the command and everything it
//! starts can be found and signalled at a timeout, a release or after a
//! restart (DM-7, DM-8).
//!
//! The service returns when the command's shell exits. A background process
//! that keeps the output pipes open does not hold the answer: output is
//! collected for a short moment after the exit, and later output is drained
//! and discarded.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::process::{self, LeaseSession};

/// How long output is still collected after the command's shell exits.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);
/// The most output kept per stream; older bytes are dropped first.
const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// What to run.
#[derive(Debug, Clone)]
pub enum Program {
    /// A shell command line, run as `$SHELL -c <line>`.
    Line {
        /// The shell.
        shell: String,
        /// The quoted command line.
        line: String,
    },
    /// A script fed to `zsh -s` on standard input.
    Script(Vec<u8>),
}

/// One command to run.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// The program.
    pub program: Program,
    /// The full environment.
    pub env: &'a BTreeMap<String, String>,
    /// The working directory.
    pub cwd: PathBuf,
    /// The deadline.
    pub timeout: Duration,
    /// The CI runner's folder, which a kill never reaches (HY-5).
    pub ci_root: PathBuf,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The shell exited.
    Exited {
        /// Its exit code, `-1` when a signal ended it.
        rc: i32,
        /// Standard output followed by standard error.
        output: String,
    },
    /// The deadline passed and the session was killed.
    TimedOut,
    /// A release preempted the command and the session was killed.
    Cancelled,
}

#[derive(Default)]
struct Stream {
    bytes: Vec<u8>,
    closed: bool,
}

/// A started command whose session is recorded.
pub struct Running {
    child: std::process::Child,
    session: LeaseSession,
    stdout: Arc<Mutex<Stream>>,
    stderr: Arc<Mutex<Stream>>,
    deadline: Instant,
    uid: u32,
    ci_root: PathBuf,
}

/// Run one command in a new process session: [`start`] then
/// [`Running::wait`].
pub fn run(
    request: &Request<'_>,
    uid: u32,
    record: impl FnOnce(LeaseSession) -> Result<(), String>,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    if cancel.load(Ordering::SeqCst) {
        return Ok(Outcome::Cancelled);
    }
    start(request, uid, record)?.wait(cancel)
}

/// Start one command in a new process session. `record` is called with the
/// session before this returns; when it fails, the session is killed and
/// the failure returned, so no command ever runs unrecorded (DM-4).
pub fn start(
    request: &Request<'_>,
    uid: u32,
    record: impl FnOnce(LeaseSession) -> Result<(), String>,
) -> Result<Running, String> {
    let (mut command, stdin) = match &request.program {
        Program::Line { shell, line } => {
            let mut command = Command::new(shell);
            command.arg("-c").arg(line);
            (command, None)
        }
        Program::Script(script) => {
            let mut command = Command::new("/bin/zsh");
            command.arg("-s");
            (command, Some(script.clone()))
        }
    };
    command
        .env_clear()
        .envs(request.env)
        .current_dir(&request.cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe and touches only the child's own
    // process state between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the command: {error}"))?;
    let pid = child.id() as i32;
    let leader_start = process::process_info(pid)
        .map(|info| info.start)
        .unwrap_or((0, 0));
    let session = LeaseSession {
        sid: pid,
        leader_start,
        group: false,
    };
    if let Err(error) = record(session) {
        kill_session(session, uid, &request.ci_root);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if let (Some(input), Some(mut handle)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = handle.write_all(&input);
        });
    }
    let stdout = Arc::new(Mutex::new(Stream::default()));
    let stderr = Arc::new(Mutex::new(Stream::default()));
    if let Some(pipe) = child.stdout.take() {
        drain(pipe, Arc::clone(&stdout));
    }
    if let Some(pipe) = child.stderr.take() {
        drain(pipe, Arc::clone(&stderr));
    }
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 3600));
    Ok(Running {
        child,
        session,
        stdout,
        stderr,
        deadline,
        uid,
        ci_root: request.ci_root.clone(),
    })
}

impl Running {
    /// The command's process session.
    pub fn session(&self) -> LeaseSession {
        self.session
    }

    /// Wait for the shell to exit, the deadline, or `cancel`. A timeout or a
    /// cancellation kills the whole session (DM-4, DM-7).
    pub fn wait(mut self, cancel: &AtomicBool) -> Result<Outcome, String> {
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    self.kill();
                    return Err(format!("cannot wait for the command: {error}"));
                }
            }
            let stop = if cancel.load(Ordering::SeqCst) {
                Some(Outcome::Cancelled)
            } else if Instant::now() >= self.deadline {
                Some(Outcome::TimedOut)
            } else {
                None
            };
            if let Some(outcome) = stop {
                self.kill();
                return Ok(outcome);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // A release that killed the session first still reads as a
        // cancellation, not as the command's own exit (DM-7).
        if cancel.load(Ordering::SeqCst) {
            return Ok(Outcome::Cancelled);
        }
        let settle = Instant::now() + DRAIN_AFTER_EXIT;
        let closed =
            |stream: &Arc<Mutex<Stream>>| stream.lock().map(|stream| stream.closed).unwrap_or(true);
        while Instant::now() < settle && !(closed(&self.stdout) && closed(&self.stderr)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let take = |stream: &Arc<Mutex<Stream>>| -> Vec<u8> {
            stream
                .lock()
                .map(|mut stream| std::mem::take(&mut stream.bytes))
                .unwrap_or_default()
        };
        let mut output = String::from_utf8_lossy(&take(&self.stdout)).to_string();
        output.push_str(&String::from_utf8_lossy(&take(&self.stderr)));
        Ok(Outcome::Exited {
            rc: status.code().unwrap_or(-1),
            output,
        })
    }

    fn kill(&mut self) {
        kill_session(self.session, self.uid, &self.ci_root);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read a pipe until it closes, keeping at most [`MAX_STREAM_BYTES`].
fn drain(mut pipe: impl Read + Send + 'static, stream: Arc<Mutex<Stream>>) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if let Ok(mut stream) = stream.lock() {
                        stream.bytes.extend_from_slice(&buffer[..count]);
                        if stream.bytes.len() > MAX_STREAM_BYTES {
                            let excess = stream.bytes.len() - MAX_STREAM_BYTES;
                            stream.bytes.drain(..excess);
                        }
                    }
                }
            }
        }
        if let Ok(mut stream) = stream.lock() {
            stream.closed = true;
        }
    });
}

/// `SIGKILL` every member of one session the lease started.
pub fn kill_session(session: LeaseSession, uid: u32, ci_root: &Path) {
    let table = process::process_table(Some(uid));
    let context = process::SweepContext {
        uid,
        lease_id: String::new(),
        lease_started: 0.0,
        sessions: vec![session],
        service_pid: std::process::id() as i32,
        service_executable: process::service_executable(),
        ci_root: ci_root.to_string_lossy().to_string(),
    };
    let verdict = process::session_members(&table, &context, &|_| None);
    process::kill_now(&verdict.matched);
}

/// Resolve a command's working directory: `work/` by default, a relative
/// path against `work/`, an absolute path as given (DM-4).
pub fn resolve_cwd(work: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    let path = match requested {
        None | Some("") => work.to_path_buf(),
        Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
        Some(path) => work.join(path),
    };
    if !path.is_dir() {
        return Err(format!("cwd {} is not a directory", path.display()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid() -> u32 {
        // SAFETY: `geteuid` only reads process state.
        unsafe { libc::geteuid() }
    }

    fn env() -> BTreeMap<String, String> {
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/tmp"),
            ("MACHINE_SERVICE_LEASE_ID", "exec-test"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
    }

    fn line(text: &str) -> Program {
        Program::Line {
            shell: "/bin/zsh".to_string(),
            line: text.to_string(),
        }
    }

    /// DM-4 (TL-2): a command runs in a new session that is recorded before
    /// the wait, with exactly the given environment and working directory.
    #[test]
    fn dm4_runs_in_a_new_recorded_session() {
        let dir = tempfile::tempdir().unwrap();
        let env = env();
        let request = Request {
            program: line("echo $MACHINE_SERVICE_LEASE_ID; pwd; echo err >&2; exit 3"),
            env: &env,
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_secs(30),
            ci_root: PathBuf::from("/nonexistent-ci"),
        };
        let mut recorded = None;
        let outcome = run(
            &request,
            uid(),
            |session| {
                recorded = Some(session);
                Ok(())
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let Outcome::Exited { rc, output } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(rc, 3);
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(output, format!("exec-test\n{}\nerr\n", canonical.display()));
        assert!(recorded.unwrap().sid > 0);
    }

    /// DM-4 (TL-2): a background process holding the output pipe does not
    /// hold the answer, and a script runs on standard input.
    #[test]
    fn dm4_background_child_does_not_block_and_scripts_run() {
        let env = env();
        let request = Request {
            program: line("sleep 30 & echo started"),
            env: &env,
            cwd: PathBuf::from("/"),
            timeout: Duration::from_secs(30),
            ci_root: PathBuf::from("/nonexistent-ci"),
        };
        let mut session = None;
        let begun = Instant::now();
        let outcome = run(
            &request,
            uid(),
            |recorded| {
                session = Some(recorded);
                Ok(())
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(begun.elapsed() < Duration::from_secs(10));
        assert!(matches!(outcome, Outcome::Exited { rc: 0, ref output } if output == "started\n"));
        kill_session(session.unwrap(), uid(), Path::new("/nonexistent-ci"));

        let script = Request {
            program: Program::Script(b"echo one\necho two\n".to_vec()),
            env: &env,
            cwd: PathBuf::from("/"),
            timeout: Duration::from_secs(30),
            ci_root: PathBuf::from("/nonexistent-ci"),
        };
        let outcome = run(&script, uid(), |_| Ok(()), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            outcome,
            Outcome::Exited {
                rc: 0,
                output: "one\ntwo\n".to_string()
            }
        );
    }

    /// DM-4 (TL-2): a timeout kills the whole session, including a child
    /// that would otherwise outlive the shell.
    #[test]
    fn dm4_timeout_kills_the_whole_session() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let env = env();
        let request = Request {
            program: line(&format!("sleep 60 & echo $! > {}; wait", pidfile.display())),
            env: &env,
            cwd: PathBuf::from("/"),
            timeout: Duration::from_millis(800),
            ci_root: PathBuf::from("/nonexistent-ci"),
        };
        let outcome = run(&request, uid(), |_| Ok(()), &AtomicBool::new(false)).unwrap();
        assert_eq!(outcome, Outcome::TimedOut);
        let child: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process::process_info(child).is_some_and(|info| !info.zombie) {
            assert!(
                Instant::now() < deadline,
                "child {child} survived the timeout"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// DM-4: a failed session record kills the command before it can run
    /// unrecorded.
    #[test]
    fn dm4_unrecorded_session_never_runs_on() {
        let env = env();
        let request = Request {
            program: line("sleep 60"),
            env: &env,
            cwd: PathBuf::from("/"),
            timeout: Duration::from_secs(30),
            ci_root: PathBuf::from("/nonexistent-ci"),
        };
        let begun = Instant::now();
        let error = run(
            &request,
            uid(),
            |_| Err("disk full".to_string()),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error, "disk full");
        assert!(begun.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn cwd_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(work.join("sub")).unwrap();
        assert_eq!(resolve_cwd(&work, None).unwrap(), work);
        assert_eq!(resolve_cwd(&work, Some("sub")).unwrap(), work.join("sub"));
        assert_eq!(
            resolve_cwd(&work, Some(dir.path().to_str().unwrap())).unwrap(),
            dir.path()
        );
        assert!(resolve_cwd(&work, Some("missing")).is_err());
    }
}
