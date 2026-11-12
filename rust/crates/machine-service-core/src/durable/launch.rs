//! A durable lease's command through the relay agent (DM-4, RA-2, RA-10),
//! whatever its entry: machine-service's own HTTP exec, or a relay client's
//! exec or script that the broker hands to this launcher (DC-219).
//!
//! On macOS the relay agent runs a command as a launchd job of its own
//! (RA-10), so the command's chain holds no process of ours, neither the
//! agent nor machine-service (DC-308, DC-315). A lease's command needs
//! more than the agent's own environment: the session environment of DM-4,
//! which holds the loaded agent keys, the lease's working directory, and a
//! record of its process group before it can start anything, so release can
//! stop it and the sweep can find what it left (DM-7, HY-3).
//!
//! machine-service therefore makes the job's folder and FIFOs in the lease's
//! state folder, and asks the agent to run the job ([`Op::Job`]); the agent
//! registers it and relays its output, and never sees its keys (DC-314).
//! A feeder thread here writes the job's `env` in two stages:
//!
//! 1. The first names no key: the job's shell writes its PID, the leader
//!    of the job's process group, to a file. The feeder records the group
//!    with the leader's start time, and only then claims the execution by
//!    removing its claim file. A release withdraws every claim first and
//!    reads the recorded groups after, so a command either is recorded
//!    before release stops the lease's groups or never starts (DC-220).
//! 2. The second, after the claim, holds the environment, the working
//!    directory and the command; a withdrawn execution gets `return 125`
//!    instead, and runs nothing.
//!
//! When the lease has loaded agent keys, the job writes its output to the
//! FIFO `raw` and its prelude's exit report to `exit`; the feeder passes the
//! output on to `out` with every key redacted, and the report to `status`
//! after the output it follows, so no relay message and no record of the
//! relay agent carries a key (DC-221). The keys reach the command through
//! its environment only, never through the relay protocol or a process's
//! arguments (AK-5).
//!
//! [`Op::Job`]: relay_agent::protocol::Op::Job

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use guest_actions::launchd::{self, JobFiles};
use relay_agent::protocol::JobRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agents::Redactor;
use super::exec::Program;
use super::process::{self, LeaseSession};
use super::session_env::SessionEnv;

/// The lease state's folder of relay jobs and process-group records.
pub const FOLDER: &str = "relay";
/// The status a job ends with when it cannot start its command; a shell
/// uses 126 and 127 for its own failures.
pub const FAILED: i32 = launchd::PRELUDE_FAILED;
/// How long the feeder waits at most for the relay agent to start the job:
/// a call's upload may come first, however slowly. A settled call removes
/// the job's folder, which ends the wait sooner.
pub const START_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long the feeder waits for the job's shell to report its PID.
const PID_DEADLINE: Duration = Duration::from_secs(10);

/// What the job runs.
#[derive(Debug, Clone, Copy)]
pub enum Launch<'a> {
    /// A command line or a script, as machine-service's own exec has it.
    Program(&'a Program),
    /// A script that arrives on the job's standard input, as the relay
    /// agent feeds a client's script to the command it runs.
    ScriptOnInput,
}

/// The files of one relay execution in a lease's state folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// The job's folder of FIFOs, which the relay agent removes.
    pub job: PathBuf,
    /// The claim, which the feeder removes before the command can start,
    /// and a release removes to stop it from starting.
    pub claim: PathBuf,
    /// The record of the job's process group.
    pub group: PathBuf,
}

/// The files of `execution` under `lease_dir`. The execution ID is hashed,
/// so any ID makes a safe file name.
pub fn files(lease_dir: &Path, execution: &str) -> Files {
    let key = service_layout::release::sha256_hex(execution.as_bytes());
    let folder = lease_dir.join(FOLDER);
    Files {
        job: folder.join(format!("{}.job", &key[..32])),
        claim: folder.join(format!("{key}.claim")),
        group: folder.join(format!("{key}.group.json")),
    }
}

fn private_folder(folder: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let folder = path.parent().unwrap_or(Path::new("."));
    private_folder(folder)?;
    let temporary = folder.join(format!(
        ".{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|_| file.sync_all()))
        .and_then(|_| std::fs::rename(&temporary, path));
    written.map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot write {}: {error}", path.display())
    })
}

/// `bytes` in single quotes for a shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The second stage's FIFO, the shell's PID file, and the redacted job's
/// own output and exit report, in the job's folder.
struct Extra {
    env2: PathBuf,
    pid: PathBuf,
    exit: PathBuf,
}

impl Extra {
    fn at(job: &JobFiles) -> Extra {
        Extra {
            env2: job.dir.join("env2"),
            pid: job.dir.join("pid"),
            exit: job.dir.join("exit"),
        }
    }
}

/// The daemon's argument that runs one job's feeder as a process of its
/// own ([`FeedBy::Process`]).
pub const FEED_ARGUMENT: &str = "--durable-feed";

/// Who feeds a job.
pub enum FeedBy<'a> {
    /// A thread of this process, which ends with it, and what takes what
    /// it has to say: for a test.
    Thread(Box<dyn Fn(&str) + Send>),
    /// A process of its own, `<executable> --durable-feed`, which outlives
    /// a restart of the daemon, so a running job keeps its redacted output
    /// and its exit report (AC-64). It is never in the job's chain of
    /// processes (DC-315). Its errors go to `log`.
    Process {
        /// The daemon's own executable.
        executable: &'a Path,
        /// The daemon's log.
        log: &'a Path,
    },
}

/// What a feeder process reads on its standard input.
#[derive(Serialize, Deserialize)]
struct FeedRequest {
    job: PathBuf,
    claim: PathBuf,
    group: PathBuf,
    first: Vec<u8>,
    second: Vec<u8>,
    /// The values to redact; with them, descriptors 3 and 4 are the
    /// readers of `raw` and `exit`.
    values: Vec<String>,
}

/// The feeder process: feed the job its standard input describes.
pub fn feed_main() -> i32 {
    let mut text = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut text) {
        eprintln!("machine-service: durable feeder: {error}");
        return 1;
    }
    let request: FeedRequest = match serde_json::from_slice(&text) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("machine-service: durable feeder: {error}");
            return 1;
        }
    };
    let job = JobFiles::at(&request.job);
    let feeder = Feeder {
        files: Files {
            job: request.job.clone(),
            claim: request.claim,
            group: request.group,
        },
        extra: Extra::at(&job),
        job,
    };
    let redacted = (!request.values.is_empty()).then(|| {
        use std::os::fd::FromRawFd;
        // SAFETY: the daemon handed this process the two readers under
        // these numbers, and nothing else here owns them.
        let (raw, exit) = unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) };
        (request.values, raw, exit)
    });
    match feeder.feed(&request.first, &request.second, redacted) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("WARN: durable job {}: {error}", request.job.display());
            1
        }
    }
}

/// Make the job for `launch` in the lease's `session`, start the feeder
/// that writes its environment, and answer what the relay agent needs to
/// run it.
pub fn prepare(
    files: &Files,
    launch: Launch<'_>,
    session: &SessionEnv,
    cwd: &Path,
    by: FeedBy<'_>,
) -> Result<JobRef, String> {
    // The job runs the shell the service resolved here, through its tool
    // table (DC-254).
    let zsh = || {
        service_layout::tools::path(service_layout::tools::Tool::Zsh)
            .map(|path| path.to_string_lossy().into_owned())
    };
    let folder = files.job.parent().unwrap_or(Path::new("."));
    private_folder(folder)?;
    let redacted = !session.secrets.is_empty();
    // A folder that exists belongs to this execution's job, made by an
    // earlier call with the same ID that may still run: it is the job
    // this call names too, and it gets no second feeder.
    let existing = || JobRef {
        dir: files.job.to_string_lossy().into_owned(),
        redacted,
    };
    let job = match JobFiles::create(&files.job) {
        Ok(job) => job,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(existing()),
        Err(error) => return Err(format!("cannot create {}: {error}", files.job.display())),
    };
    let extra = Extra::at(&job);
    let prepared = (|| {
        launchd::make_fifo(&extra.env2).map_err(|error| error.to_string())?;
        let (shell, command) = match launch {
            Launch::Program(Program::Line { shell, line }) => (
                service_layout::tools::program(shell)?
                    .to_string_lossy()
                    .into_owned(),
                line.clone(),
            ),
            Launch::Program(Program::Script(bytes)) => {
                let script = job.dir.join("script");
                private_file(&script, bytes)?;
                let zsh = zsh()?;
                let line = format!(
                    "exec {} -s < {}",
                    quote(&zsh),
                    quote(&script.to_string_lossy())
                );
                (zsh, line)
            }
            Launch::ScriptOnInput => {
                let zsh = zsh()?;
                let line = format!("exec {} -s", quote(&zsh));
                (zsh, line)
            }
        };
        let values: Vec<String> = session
            .secrets
            .iter()
            .map(|secret| secret.expose().to_string())
            .collect();
        let redactor = Redactor::new(&values);
        // The redacted job's own output and report have their readers
        // before the agent registers it, so launchd's open and the
        // prelude's never block.
        let redacted = if redactor.is_empty() {
            None
        } else {
            launchd::make_fifo(&job.raw).map_err(|error| error.to_string())?;
            launchd::make_fifo(&extra.exit).map_err(|error| error.to_string())?;
            let raw = open_reader(&job.raw)?;
            let exit = open_reader(&extra.exit)?;
            Some((values, raw, exit))
        };
        private_file(&files.claim, b"")?;
        let mut first = String::new();
        if redacted.is_some() {
            first.push_str(&format!(
                "exec 9>{} || return 125\n",
                quote(&extra.exit.to_string_lossy())
            ));
        }
        first.push_str(&format!(
            "printf '%s\\n' \"$$\" > {} || return 125\n\
             eval \"$(/bin/cat {})\" || return 125\n",
            quote(&extra.pid.to_string_lossy()),
            quote(&extra.env2.to_string_lossy())
        ));
        let environment: Vec<(OsString, OsString)> = session
            .vars
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        let second = launchd::env_script(&environment, cwd, Path::new(&shell), &command);
        Ok::<_, String>((first, second, redacted))
    })();
    let (first, second, redacted) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            job.remove();
            let _ = std::fs::remove_file(&files.claim);
            return Err(error);
        }
    };
    let reference = JobRef {
        dir: job.dir.to_string_lossy().into_owned(),
        redacted: redacted.is_some(),
    };
    let started = match by {
        FeedBy::Thread(log) => {
            let feeder = Feeder {
                files: files.clone(),
                job: job.clone(),
                extra,
            };
            std::thread::Builder::new()
                .name("durable-job-feeder".into())
                .spawn(move || {
                    if let Err(error) = feeder.feed(first.as_bytes(), &second, redacted) {
                        log(&format!(
                            "WARN: durable job {}: {error}",
                            feeder.job.dir.display()
                        ));
                    }
                })
                .map(|_| ())
                .map_err(|error| format!("cannot start the job's feeder: {error}"))
        }
        FeedBy::Process { executable, log } => {
            spawn_feeder(executable, log, files, first.into_bytes(), second, redacted)
        }
    };
    if let Err(error) = started {
        job.remove();
        let _ = std::fs::remove_file(&files.claim);
        return Err(error);
    }
    Ok(reference)
}

/// Start `<executable> --durable-feed` in a process group of its own, with
/// its request on standard input and, for a redacted job, the readers of
/// `raw` and `exit` as descriptors 3 and 4.
fn spawn_feeder(
    executable: &Path,
    log: &Path,
    files: &Files,
    first: Vec<u8>,
    second: Vec<u8>,
    redacted: Option<(Vec<String>, File, File)>,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let (values, readers) = match redacted {
        Some((values, raw, exit)) => (values, Some((raw, exit))),
        None => (Vec::new(), None),
    };
    let request = FeedRequest {
        job: files.job.clone(),
        claim: files.claim.clone(),
        group: files.group.clone(),
        first,
        second,
        values,
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    let errors = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    let mut command = std::process::Command::new(executable);
    command
        .arg(FEED_ARGUMENT)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(errors)
        .process_group(0);
    if let Some((raw, exit)) = &readers {
        let (raw, exit) = (raw.as_raw_fd(), exit.as_raw_fd());
        // SAFETY: only async-signal-safe calls between fork and exec. Each
        // reader is first copied above 4, so neither move overwrites the
        // other and none is a `dup2` onto itself, which would keep
        // close-on-exec; the copies under 3 and 4 have it cleared.
        unsafe {
            command.pre_exec(move || {
                let high = |fd: i32| libc::fcntl(fd, libc::F_DUPFD, 10);
                let (raw, exit) = (high(raw), high(exit));
                if raw < 0
                    || exit < 0
                    || libc::dup2(raw, 3) < 0
                    || libc::dup2(exit, 4) < 0
                    || libc::fcntl(3, libc::F_SETFD, 0) < 0
                    || libc::fcntl(4, libc::F_SETFD, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the job's feeder: {error}"))?;
    // This process's copies of the readers go once the feeder holds its own.
    drop(readers);
    let mut stdin = child
        .stdin
        .take()
        .ok_or("the feeder has no standard input")?;
    stdin
        .write_all(&bytes)
        .map_err(|error| format!("cannot hand the feeder its job: {error}"))?;
    drop(stdin);
    // The feeder is reaped by a thread, so it never lingers as a zombie of
    // the daemon; after a restart of the daemon, launchd reaps it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn open_reader(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))
}

struct Feeder {
    files: Files,
    job: JobFiles,
    extra: Extra,
}

impl Feeder {
    fn withdrawn(&self) -> bool {
        !self.job.dir.exists()
    }

    fn feed(
        &self,
        first: &[u8],
        second: &[u8],
        redacted: Option<(Vec<String>, File, File)>,
    ) -> Result<(), String> {
        // Stage 1, once the agent has started the job.
        self.write_fifo(&self.job.env, first, START_DEADLINE)
            .map_err(|error| format!("the job never read its environment: {error}"))?;
        let started = Instant::now();
        let pid = loop {
            let text = std::fs::read_to_string(&self.extra.pid).unwrap_or_default();
            if let Ok(pid) = text.trim().parse::<i32>() {
                break pid;
            }
            if started.elapsed() > PID_DEADLINE || self.withdrawn() {
                return Err("the job's shell never reported its PID".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        // The record, then the claim (DC-220).
        let recorded = process::process_info(pid)
            .ok_or_else(|| format!("cannot read process group {pid}"))
            .and_then(|leader| {
                let record = LeaseSession {
                    sid: pid,
                    leader_start: leader.start,
                    group: true,
                };
                let bytes =
                    serde_json::to_vec(&record.to_json()).map_err(|error| error.to_string())?;
                private_file(&self.files.group, &bytes)
            });
        let claimed = recorded.is_ok() && std::fs::remove_file(&self.files.claim).is_ok();
        let stage = if claimed {
            second
        } else {
            b"return 125\n".as_slice()
        };
        self.write_fifo(&self.extra.env2, stage, PID_DEADLINE)
            .map_err(|error| format!("the job's shell never read its command: {error}"))?;
        // A redacted job's report reaches the agent through here, also the
        // launch failure of a job that runs nothing.
        if let Some((values, raw, exit)) = redacted {
            self.relay(&values, raw, exit)?;
        }
        recorded?;
        if !claimed {
            return Err("withdrawn before it started".into());
        }
        Ok(())
    }

    /// Write `bytes` into the FIFO `path` once its reader opens it.
    fn write_fifo(&self, path: &Path, bytes: &[u8], deadline: Duration) -> Result<(), String> {
        let started = Instant::now();
        let file = loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) => break file,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    if started.elapsed() > deadline {
                        return Err(format!("not opened within {} s", deadline.as_secs()));
                    }
                    if self.withdrawn() {
                        return Err("withdrawn".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        self.write_all(&file, bytes)
    }

    /// Write `bytes` to the non-blocking `file`, waiting while it is full,
    /// until the job's folder goes.
    fn write_all(&self, file: &File, mut bytes: &[u8]) -> Result<(), String> {
        let mut file = file;
        while !bytes.is_empty() {
            match file.write(bytes) {
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if self.withdrawn() {
                        return Err("the job's folder went".into());
                    }
                    poll(&[(file.as_raw_fd(), libc::POLLOUT)], 200);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }

    /// Pass the job's output on to `out` redacted, and the prelude's exit
    /// report on to `status` after the output it follows, until the job's
    /// folder goes or every writer has closed both.
    fn relay(&self, values: &[String], mut raw: File, mut exit: File) -> Result<(), String> {
        let open = |path: &Path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .map_err(|error| format!("cannot open {}: {error}", path.display()))
        };
        let out = open(&self.job.out)?;
        let status = open(&self.job.status)?;
        let mut redactor = Redactor::new(values);
        let mut buffer = vec![0u8; 64 << 10];
        let mut report = Vec::new();
        let (mut raw_open, mut exit_open) = (true, true);
        while (raw_open || exit_open) && !self.withdrawn() {
            let mut fds = Vec::new();
            if raw_open {
                fds.push((raw.as_raw_fd(), libc::POLLIN));
            }
            if exit_open {
                fds.push((exit.as_raw_fd(), libc::POLLIN));
            }
            poll(&fds, 200);
            if raw_open {
                raw_open = self.pass(&mut raw, &mut buffer, &mut redactor, &out)?;
            }
            if exit_open {
                match exit.read(&mut buffer) {
                    Ok(0) => exit_open = false,
                    Ok(count) => {
                        report.extend_from_slice(&buffer[..count]);
                        if report.contains(&b'\n') {
                            // What the command wrote before it ended is in
                            // `raw` by now: it goes first, whole.
                            if raw_open {
                                raw_open = self.pass(&mut raw, &mut buffer, &mut redactor, &out)?;
                            }
                            // The redactor's held tail goes before the
                            // report; what the command left running is
                            // redacted afresh after it.
                            let ended = std::mem::replace(&mut redactor, Redactor::new(values));
                            self.write_all(&out, &ended.finish())?;
                            self.write_all(&status, &report)?;
                            report.clear();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        self.write_all(&out, &redactor.finish())?;
        Ok(())
    }

    /// Pass what `raw` holds now on to `out`; false once it has ended.
    fn pass(
        &self,
        raw: &mut File,
        buffer: &mut [u8],
        redactor: &mut Redactor,
        out: &File,
    ) -> Result<bool, String> {
        loop {
            match raw.read(buffer) {
                Ok(0) => return Ok(false),
                Ok(count) => {
                    let redacted = redactor.push(&buffer[..count]);
                    self.write_all(out, &redacted)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

fn poll(fds: &[(i32, i16)], timeout_ms: i32) {
    let mut fds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        })
        .collect();
    // SAFETY: `fds` is a valid array of `pollfd` of the length given.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
}

/// A job this process registered itself, when no relay agent is there to
/// (RA-8, DC-318): the same job and feeder as the relay agent's, so the
/// command is never this process's child either (DC-315).
pub struct LocalJob {
    files: Files,
    spawned: guest_actions::Spawned,
}

/// How long a local job's start waits for its group's record.
const RECORD_WAIT: Duration = Duration::from_secs(30);

/// Make `execution`'s job for `launch` and register it in this user's GUI
/// domain, its feeder a process of `executable`; answer once its group is
/// recorded, so a release that starts after this returns finds it (DC-220).
pub fn start_local(
    files: &Files,
    launch: Launch<'_>,
    session: &SessionEnv,
    cwd: &Path,
    executable: &Path,
    log: &Path,
    agent: &str,
) -> Result<LocalJob, String> {
    let launchd = launchd::Launchd::resolve()?;
    let shell = service_layout::tools::path(service_layout::tools::Tool::Zsh)?;
    let job = prepare(
        files,
        launch,
        session,
        cwd,
        FeedBy::Process { executable, log },
    )?;
    let key = files
        .job
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label = launchd::label_for(agent, &key);
    let spawned = guest_actions::spawn_job(
        &launchd,
        &label,
        &shell,
        Path::new(&job.dir),
        "",
        false,
        launchd::EnvWriter::Elsewhere {
            redacted: job.redacted,
        },
    )
    .map_err(|error| {
        withdraw(files);
        format!("cannot start the job: {error}")
    })?;
    let deadline = Instant::now() + RECORD_WAIT;
    while !files.group.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(LocalJob {
        files: files.clone(),
        spawned,
    })
}

impl LocalJob {
    /// Wait for the job: its output, its end, the deadline, or `cancel`,
    /// which a release sets; then remove what it left of its files.
    pub fn wait(
        self,
        timeout: Duration,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> super::exec::Outcome {
        use std::sync::atomic::Ordering;
        let killer = self.spawned.killer();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A release preempts the job: the watcher stops it, as the deadline
        // would.
        std::thread::scope(|scope| {
            let (stopped, finished) = (cancelled.clone(), done.clone());
            scope.spawn(move || {
                while !finished.load(Ordering::SeqCst) {
                    if cancel.load(Ordering::SeqCst) {
                        stopped.store(true, Ordering::SeqCst);
                        killer.kill();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            let mut output = Vec::new();
            let exit = self.spawned.supervise_from(None, timeout, |bytes| {
                output.extend_from_slice(bytes);
                true
            });
            done.store(true, Ordering::SeqCst);
            withdraw(&self.files);
            // A release sets `cancel` and stops the lease's groups itself, so
            // the job may end before the watcher sees the flag: the flag
            // decides.
            if cancelled.load(Ordering::SeqCst) || cancel.load(Ordering::SeqCst) {
                super::exec::Outcome::Cancelled
            } else if exit.timed_out {
                super::exec::Outcome::TimedOut
            } else {
                super::exec::Outcome::Exited {
                    rc: exit.rc.unwrap_or(-1),
                    output: String::from_utf8_lossy(&output).into_owned(),
                }
            }
        })
    }
}

/// The process group a job recorded, once it has.
pub fn recorded_group(path: &Path) -> Option<LeaseSession> {
    let text = std::fs::read_to_string(path).ok()?;
    LeaseSession::from_json(&serde_json::from_str::<Value>(&text).ok()?)
}

/// Every process group recorded under `lease_dir`.
pub fn recorded_groups(lease_dir: &Path) -> Vec<LeaseSession> {
    let Ok(entries) = std::fs::read_dir(lease_dir.join(FOLDER)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".group.json"))
        })
        .filter_map(|path| recorded_group(&path))
        .collect()
}

/// Remove what one settled execution left: its claim and its job's folder,
/// never its process group's record, which release still needs.
pub fn withdraw(files: &Files) {
    let _ = std::fs::remove_file(&files.claim);
    let _ = std::fs::remove_dir_all(&files.job);
}

/// Remove every claim no feeder has taken yet, so a job the relay agent
/// starts after a release runs nothing (DM-7). The jobs' folders stay, so
/// the jobs that run keep their output until release stops them.
pub fn withdraw_requests(lease_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(lease_dir.join(FOLDER)) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // `.request.json` and `.script` are what the launcher before RA-10
        // left.
        if name.ends_with(".claim") || name.ends_with(".request.json") || name.ends_with(".script")
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn session() -> SessionEnv {
        SessionEnv {
            vars: BTreeMap::from([("KEY".to_string(), "secret".to_string())]),
            secrets: vec![super::super::agents::Secret::new("secret")],
        }
    }

    #[test]
    fn a_job_is_private_and_its_reference_names_no_key() {
        let lease = tempfile::tempdir().unwrap();
        let files = files(lease.path(), "exec-1");
        let program = Program::Line {
            shell: "/bin/zsh".into(),
            line: "echo hi".into(),
        };
        let job = prepare(
            &files,
            Launch::Program(&program),
            &session(),
            lease.path(),
            FeedBy::Thread(Box::new(|_| {})),
        )
        .unwrap();
        assert!(job.redacted);
        assert!(!job.dir.contains("secret"));
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&files.job), 0o700);
        assert_eq!(mode(files.job.parent().unwrap()), 0o700);
        assert!(files.claim.exists());
        assert_ne!(files.job, super::files(lease.path(), "exec-2").job);
        withdraw_requests(lease.path());
        assert!(!files.claim.exists());
        assert!(files.job.exists());
        withdraw(&files);
        assert!(!files.job.exists());
    }

    /// One execution's leftovers are withdrawn alone, and its group's
    /// record stays for release.
    #[test]
    fn one_executions_withdrawal() {
        let lease = tempfile::tempdir().unwrap();
        let one = files(lease.path(), "exec-1");
        let two = files(lease.path(), "exec-2");
        let plain = SessionEnv {
            vars: BTreeMap::new(),
            secrets: Vec::new(),
        };
        for files in [&one, &two] {
            let job = prepare(
                files,
                Launch::ScriptOnInput,
                &plain,
                lease.path(),
                FeedBy::Thread(Box::new(|_| {})),
            )
            .unwrap();
            assert!(!job.redacted);
        }
        std::fs::write(&one.group, b"{}").unwrap();
        withdraw(&one);
        assert!(!one.job.exists() && !one.claim.exists());
        assert!(one.group.exists());
        assert!(two.job.exists() && two.claim.exists());
    }
}
