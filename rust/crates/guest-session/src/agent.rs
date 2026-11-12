//! The guest side of a guest session (`docs/arch/guest-session.md`, GS-3).
//!
//! [`serve`] reads frames from the host and answers them until the input
//! ends. Each request has one commit point, the host's end frame: a command
//! starts only after its standard input arrived whole and matched its
//! SHA-256, and a written file is renamed into place only after its bytes
//! matched. When the input ends or a frame is malformed, every running
//! command's process group is killed and every unfinished file is removed.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use guest_actions::{kill_group, spawn_shell, ActionError, AtomicFile};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub use guest_actions::{hex, resolve};

use crate::frame::{read_frame, write_frame, Frame, FrameError, Kind};

/// The protocol version the agent announces in its hello frame.
pub const PROTOCOL: u64 = 2;
/// How often the agent sends an alive frame.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// The largest standard input an exec may carry.
pub const MAX_STDIN: usize = 256 << 20;
/// The size of the data frames the agent sends.
pub const CHUNK: usize = 256 << 10;

/// How the agent runs commands and resolves paths.
#[derive(Clone, Debug)]
pub struct AgentOptions {
    /// The shell that runs each command line with `-c`, as `sshd` does.
    pub shell: PathBuf,
    /// The directory relative guest paths are resolved against, as `scp` does.
    pub home: PathBuf,
    /// The SHA-256 of the agent's own executable, announced in the hello frame.
    pub build: String,
    /// The interval of alive frames, announced in the hello frame; `None`
    /// sends none.
    pub heartbeat: Option<Duration>,
}

impl AgentOptions {
    /// Options from the process environment: `$SHELL` (else `/bin/sh`),
    /// `$HOME` (else `/`), the hash of the running executable, and alive
    /// frames every [`HEARTBEAT`].
    pub fn from_environment() -> AgentOptions {
        AgentOptions {
            shell: guest_actions::user_shell(),
            home: guest_actions::user_home(),
            build: guest_actions::executable_sha256(),
            heartbeat: Some(HEARTBEAT),
        }
    }
}

/// The frame writer shared by the request loop and the request threads.
#[derive(Clone)]
struct Sink(Arc<Mutex<Box<dyn Write + Send>>>);

impl Sink {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        let mut writer = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        write_frame(&mut **writer, frame)
    }

    fn fail(&self, request: u64, error: &ActionError) {
        self.error(request, error.code, &error.message);
    }

    fn error(&self, request: u64, code: &str, message: &str) {
        let _ = self.send(&Frame::json(
            Kind::Error,
            request,
            &json!({"code": code, "message": message}),
        ));
    }
}

/// A command that is running.
struct Running {
    group: i32,
    cancelled: AtomicBool,
}

/// A request waiting for its end frame.
enum Pending {
    Exec {
        command: String,
        connect_stdin: bool,
        timeout: Duration,
        input: Vec<u8>,
    },
    /// Dropping the file removes its temporary sibling.
    Write(AtomicFile),
}

/// Serve one guest session until `input` ends.
///
/// Returns an error when a frame is malformed or the host can no longer be
/// written to; either way the session is over and everything it started has
/// been stopped.
pub fn serve(
    mut input: impl Read,
    output: impl Write + Send + 'static,
    options: AgentOptions,
) -> Result<(), FrameError> {
    let sink = Sink(Arc::new(Mutex::new(Box::new(output))));
    let running: Arc<Mutex<HashMap<u64, Arc<Running>>>> = Arc::default();
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let heartbeat_ms = options
        .heartbeat
        .map(|interval| interval.as_millis() as u64);
    sink.send(&Frame::json(
        Kind::Hello,
        0,
        &json!({
            "protocol": PROTOCOL,
            "build": options.build,
            "os": std::env::consts::OS,
            "heartbeat_ms": heartbeat_ms,
        }),
    ))?;
    let (stop, stopped) = mpsc::channel::<()>();
    if let Some(interval) = options.heartbeat {
        let sink = sink.clone();
        thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                if sink.send(&Frame::new(Kind::Alive, 0, Vec::new())).is_err() {
                    return;
                }
            }
        });
    }
    let result = (|| -> Result<(), FrameError> {
        while let Some(frame) = read_frame(&mut input)? {
            handle(&frame, &sink, &running, &mut pending, &options)?;
        }
        Ok(())
    })();
    drop(stop);
    pending.clear();
    for process in running.lock().unwrap_or_else(|p| p.into_inner()).values() {
        kill_group(process.group);
    }
    result
}

fn handle(
    frame: &Frame,
    sink: &Sink,
    running: &Arc<Mutex<HashMap<u64, Arc<Running>>>>,
    pending: &mut HashMap<u64, Pending>,
    options: &AgentOptions,
) -> Result<(), FrameError> {
    let id = frame.request;
    let busy = |pending: &HashMap<u64, Pending>| {
        pending.contains_key(&id) || running.lock().unwrap().contains_key(&id)
    };
    match frame.kind {
        Kind::Ping => sink.send(&Frame::new(Kind::Pong, id, frame.payload.clone()))?,
        Kind::Exec | Kind::Write | Kind::Read if id == 0 || busy(pending) => {
            sink.error(id, "duplicate_request", "request ID is 0 or already in use");
        }
        Kind::Exec => {
            let value = frame.value()?;
            let command = value.get("command").and_then(Value::as_str);
            let timeout = value.get("timeout_s").and_then(Value::as_f64);
            match (command, timeout) {
                (Some(command), Some(timeout)) if timeout > 0.0 && timeout.is_finite() => {
                    pending.insert(
                        id,
                        Pending::Exec {
                            command: command.to_string(),
                            connect_stdin: value.get("stdin").and_then(Value::as_bool)
                                == Some(true),
                            timeout: Duration::from_secs_f64(timeout),
                            input: Vec::new(),
                        },
                    );
                }
                _ => sink.error(
                    id,
                    "invalid_request",
                    "exec needs command and a positive timeout_s",
                ),
            }
        }
        Kind::Write => {
            let value = frame.value()?;
            match open_write(&value, &options.home, id) {
                Ok(write) => {
                    pending.insert(id, Pending::Write(write));
                }
                Err(error) => sink.fail(id, &error),
            }
        }
        Kind::Read => {
            let value = frame.value()?;
            match value.get("path").and_then(Value::as_str) {
                Some(path) => {
                    let path = resolve(path, &options.home);
                    let sink = sink.clone();
                    thread::spawn(move || read_file(&sink, id, &path));
                }
                None => sink.error(id, "invalid_request", "read needs path"),
            }
        }
        Kind::Data => match pending.get_mut(&id) {
            Some(Pending::Exec { input, .. }) => {
                if input.len() + frame.payload.len() > MAX_STDIN {
                    pending.remove(&id);
                    sink.error(id, "too_large", "standard input exceeds the limit");
                } else {
                    input.extend_from_slice(&frame.payload);
                }
            }
            Some(Pending::Write(file)) => {
                if let Err(error) = file.append(&frame.payload) {
                    pending.remove(&id);
                    sink.fail(id, &error);
                }
            }
            // A request that already failed may still have data in flight.
            None => {}
        },
        Kind::End => {
            let value = frame.value()?;
            let declared = (
                value.get("size").and_then(Value::as_u64),
                value
                    .get("sha256")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            );
            match pending.remove(&id) {
                Some(Pending::Exec {
                    command,
                    connect_stdin,
                    timeout,
                    input,
                }) => {
                    let actual = (Some(input.len() as u64), Some(hex(&Sha256::digest(&input))));
                    if actual != declared {
                        sink.error(
                            id,
                            "checksum_mismatch",
                            "standard input does not match its end frame",
                        );
                    } else {
                        let stdin = connect_stdin.then_some(input);
                        start_exec(sink, running, id, options, &command, stdin, timeout);
                    }
                }
                Some(Pending::Write(file)) => {
                    let finished = match declared {
                        (Some(size), Some(sha256)) => file
                            .seal(size, &sha256)
                            .and_then(guest_actions::Sealed::rename),
                        _ => Err(ActionError::new(
                            "checksum_mismatch",
                            "file does not match its end frame",
                        )),
                    };
                    match finished {
                        Ok(digest) => sink.send(&Frame::json(
                            Kind::Done,
                            id,
                            &json!({"size": digest.size, "sha256": digest.sha256}),
                        ))?,
                        Err(error) => sink.fail(id, &error),
                    }
                }
                None => {}
            }
        }
        Kind::Cancel => {
            if let Some(process) = running.lock().unwrap().get(&id) {
                process.cancelled.store(true, Ordering::SeqCst);
                kill_group(process.group);
            } else if pending.remove(&id).is_some() {
                sink.error(id, "cancelled", "cancelled before its end frame");
            }
        }
        Kind::Hello
        | Kind::Pong
        | Kind::Alive
        | Kind::Output
        | Kind::Exit
        | Kind::Done
        | Kind::Error => {
            sink.error(
                id,
                "unexpected_frame",
                "this frame type is sent by the agent, not to it",
            );
        }
    }
    Ok(())
}

fn open_write(value: &Value, home: &Path, id: u64) -> Result<AtomicFile, ActionError> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ActionError::new("invalid_request", "write needs path"))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mode = value
        .get("mode")
        .and_then(Value::as_u64)
        .map(|mode| mode as u32);
    AtomicFile::create(path, name, mode, home, "guest-session", id)
}

fn read_file(sink: &Sink, id: u64, path: &Path) {
    let file = match guest_actions::open_read(path) {
        Ok(file) => file,
        Err(error) => return sink.fail(id, &error),
    };
    let sent = guest_actions::read_chunks(file, CHUNK, |bytes| {
        sink.send(&Frame::new(Kind::Data, id, bytes.to_vec()))
            .is_ok()
    });
    match sent {
        Ok(digest) => {
            let _ = sink.send(&Frame::json(
                Kind::End,
                id,
                &json!({"size": digest.size, "sha256": digest.sha256}),
            ));
        }
        Err(error) if error.code == "stopped" => {}
        Err(error) => sink.fail(id, &error),
    }
}

fn start_exec(
    sink: &Sink,
    running: &Arc<Mutex<HashMap<u64, Arc<Running>>>>,
    id: u64,
    options: &AgentOptions,
    command: &str,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) {
    let spawned = match spawn_shell(&options.shell, &options.home, command, stdin.is_some()) {
        Ok(spawned) => spawned,
        Err(error) => return sink.error(id, "spawn_failed", &error.to_string()),
    };
    let process = Arc::new(Running {
        group: spawned.group,
        cancelled: AtomicBool::new(false),
    });
    running.lock().unwrap().insert(id, Arc::clone(&process));
    let sink = sink.clone();
    let running = Arc::clone(running);
    thread::spawn(move || {
        let exit = spawned.supervise(stdin, timeout, |bytes| {
            sink.send(&Frame::new(Kind::Output, id, bytes.to_vec()))
                .is_ok()
        });
        running.lock().unwrap().remove(&id);
        let _ = sink.send(&Frame::json(
            Kind::Exit,
            id,
            &json!({
                "rc": exit.rc,
                "signal": exit.signal,
                "timed_out": exit.timed_out,
                "cancelled": process.cancelled.load(Ordering::SeqCst),
            }),
        ));
    });
}
