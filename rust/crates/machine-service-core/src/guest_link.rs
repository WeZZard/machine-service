//! Guest sessions for exec, push and pull (`docs/arch/guest-session.md`).
//!
//! With `MACHINE_SERVICE_GUEST_LINK=ssh-session`, machine-service keeps one guest
//! session per lease (GS-1) on a channel the [`crate::host::Host`] opens (GS-2), to the
//! `guest-session-agent` it uploads into the guest (GS-3). Requests follow the
//! GS-5 resend rules: a request that never reached its commit point is sent
//! once more on a new session, a read is always sent once more, and an exec or
//! write past its commit point is reported uncertain and never sent again.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use guest_session::agent::{hex, PROTOCOL};
use guest_session::client::{Op, Session, SessionError};
use sha2::{Digest, Sha256};

use crate::error::{OpError, OpResult};
use crate::service::Service;

/// Time a new session gets to send its hello.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// A session idle longer than this is pinged before reuse (GS-1).
const IDLE_CHECK: Duration = Duration::from_secs(10);
/// Time the ping of an idle session gets.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Time a closed channel gets to exit before it is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Time the agent upload gets.
const UPLOAD_TIMEOUT_S: u64 = 120;
/// Bytes of a channel's standard error kept for diagnostics.
const DIAGNOSTICS_LIMIT: usize = 4096;

/// The remote command that starts the uploaded agent, or exits 127 when it is
/// missing.
pub const START_COMMAND: &str = "sh -c 'a=\"$HOME/.vm-service-session/agent\"; \
     [ -x \"$a\" ] || exit 127; exec \"$a\" serve'";

/// The remote command that stores the agent atomically from standard input,
/// in a private directory, as the console agent upload does.
pub const UPLOAD_COMMAND: &str = "sh -c 'set -e; d=\"$HOME/.vm-service-session\"; \
     mkdir -p \"$d\"; chmod 700 \"$d\"; t=\"$d/agent.tmp.$$\"; \
     (umask 077; cat >\"$t\"); chmod 700 \"$t\"; mv -f \"$t\" \"$d/agent\"'";

/// A process carrying a guest session's byte stream.
pub trait ChannelProcess: Send {
    /// Wait up to `grace` for the process to exit after its input closed,
    /// then kill it. Never fails.
    fn finish(&mut self, grace: Duration);

    /// What the process wrote to its standard error, bounded.
    fn diagnostics(&self) -> String {
        String::new()
    }

    /// A handle that ends the channel at once from another thread, used when
    /// the agent falls silent (GS-1).
    fn abort_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
}

/// A byte stream to the guest: the channel process's standard output and
/// input, and the process itself.
pub struct GuestChannel {
    /// Bytes from the agent.
    pub reader: Box<dyn Read + Send>,
    /// Bytes to the agent.
    pub writer: Box<dyn Write + Send>,
    /// The process carrying the stream.
    pub process: Box<dyn ChannelProcess>,
}

/// Where a lease's guest is reached.
pub struct GuestTarget<'a> {
    /// The guest's IP address.
    pub ip: &'a str,
    /// The lease's SSH user.
    pub user: &'a str,
    /// The lease's key directory.
    pub key_dir: &'a Path,
    /// The guest's kind, `linux` or `macos`.
    pub kind: &'a str,
}

/// An open guest session and the process carrying it.
pub struct LiveSession {
    session: Session,
    process: Box<dyn ChannelProcess>,
}

impl LiveSession {
    /// Close the session: the agent sees its input end and stops what it
    /// started; the channel process is killed if it does not exit.
    pub fn close(self) {
        let LiveSession {
            session,
            mut process,
        } = self;
        session.close();
        process.finish(CLOSE_GRACE);
    }
}

/// Why a request on a guest session failed.
#[derive(Debug)]
pub enum GuestError {
    /// No session could be opened; nothing was sent.
    Open(OpError),
    /// The request failed on an open session.
    Session(SessionError),
}

/// The path of the guest session agent for a guest of `kind`.
///
/// `VM_GUEST_SESSION_AGENT` wins; otherwise `guest-session-agent-<kind>`
/// beside the running executable, then the host-native
/// `guest-session-agent`, as the console agent is resolved. The installer
/// installs both builds there from the same layout (IN-1, DC-79).
pub fn agent_path_for(kind: &str) -> OpResult<PathBuf> {
    if let Some(path) = std::env::var_os("VM_GUEST_SESSION_AGENT") {
        return Ok(PathBuf::from(path));
    }
    service_layout::guest_build_beside_daemon(service_layout::Executable::GuestSessionAgent, kind)
        .ok_or_else(|| OpError::new("guest session agent unavailable"))
}

/// Read the guest session agent for a guest of `kind`.
pub fn agent_bytes_for(kind: &str) -> OpResult<Vec<u8>> {
    let path = agent_path_for(kind)?;
    std::fs::read(&path).map_err(|error| {
        OpError::new(format!(
            "guest session agent unavailable at {}: {error}",
            path.display()
        ))
    })
}

/// The real channel: an `ssh` child whose standard input and output carry
/// the session.
pub struct ChildChannel {
    child: Arc<Mutex<Child>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    status: Option<std::process::ExitStatus>,
}

impl ChildChannel {
    /// Spawn `command` with piped standard streams.
    pub fn spawn(mut command: Command) -> OpResult<GuestChannel> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| OpError::new(format!("guest session channel failed: {error}")))?;
        let writer = child.stdin.take().expect("stdin was piped");
        let reader = child.stdout.take().expect("stdout was piped");
        let mut errors = child.stderr.take().expect("stderr was piped");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            while let Ok(count) = errors.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut kept = collected.lock().expect("stderr buffer");
                kept.extend_from_slice(&buffer[..count]);
                let excess = kept.len().saturating_sub(DIAGNOSTICS_LIMIT);
                kept.drain(..excess);
            }
        });
        Ok(GuestChannel {
            reader: Box::new(reader),
            writer: Box::new(writer),
            process: Box::new(ChildChannel {
                child: Arc::new(Mutex::new(child)),
                stderr,
                status: None,
            }),
        })
    }
}

impl ChannelProcess for ChildChannel {
    fn finish(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        loop {
            let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    break;
                }
                Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn diagnostics(&self) -> String {
        let kept = self.stderr.lock().expect("stderr buffer");
        let stderr = String::from_utf8_lossy(&kept).trim().to_string();
        // START_COMMAND exits 127 when the guest has no agent.
        let status = match self.status.and_then(|status| status.code()) {
            Some(127) => "no agent in the guest (exit 127)".to_string(),
            Some(code) => format!("the channel exited {code}"),
            None => String::new(),
        };
        [status, stderr]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn abort_handle(&self) -> Option<Box<dyn FnOnce() + Send>> {
        let child = Arc::clone(&self.child);
        Some(Box::new(move || {
            // A child already waited for is not signalled again.
            let _ = child.lock().unwrap_or_else(|p| p.into_inner()).kill();
        }))
    }
}

impl Drop for ChildChannel {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(None) = child.try_wait() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Service {
    /// Run `request` on the VM's guest session, opening one when needed and
    /// applying the GS-5 resend rules for `op`.
    ///
    /// The caller holds the VM's operation lock. The session watches the VM's
    /// release flag, so a release cancels a running exec and closes the
    /// session under any other request.
    pub fn with_guest_session<T>(
        &self,
        vm: &str,
        target: &GuestTarget<'_>,
        op: Op,
        mut request: impl FnMut(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, GuestError> {
        let flag = self.release_flag(vm);
        let mut resent = false;
        loop {
            let mut live = self
                .usable_session(vm, target, &flag)
                .map_err(GuestError::Open)?;
            let result = request(&mut live.session);
            let intact = live.session.broken().is_none() && !flag.load(Ordering::SeqCst);
            if intact {
                self.guest_sessions
                    .lock()
                    .expect("guest session map is never poisoned")
                    .insert(vm.to_string(), live);
            } else {
                live.close();
            }
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let again = !resent && !flag.load(Ordering::SeqCst) && error.may_resend(op);
                    if !again {
                        return Err(GuestError::Session(error));
                    }
                    self.log(&format!(
                        "{vm}: resending a {op:?} on a new guest session: {error}"
                    ));
                    resent = true;
                }
            }
        }
    }

    /// Close the VM's guest session, if it has one.
    pub fn close_guest_session(&self, vm: &str) {
        let live = self
            .guest_sessions
            .lock()
            .expect("guest session map is never poisoned")
            .remove(vm);
        if let Some(live) = live {
            live.close();
            self.log(&format!("{vm}: guest session closed"));
        }
    }

    /// The VM's session if it is still usable, else a new one.
    fn usable_session(
        &self,
        vm: &str,
        target: &GuestTarget<'_>,
        flag: &Arc<AtomicBool>,
    ) -> OpResult<LiveSession> {
        let existing = self
            .guest_sessions
            .lock()
            .expect("guest session map is never poisoned")
            .remove(vm);
        if let Some(mut live) = existing {
            let usable = live.session.broken().is_none()
                && (live.session.idle() < IDLE_CHECK || live.session.ping(PING_TIMEOUT).is_ok());
            if usable {
                return Ok(live);
            }
            let reason = live
                .session
                .broken()
                .unwrap_or("no answer to a ping")
                .to_string();
            live.close();
            self.log(&format!("{vm}: replacing the guest session: {reason}"));
        }
        self.open_session(vm, target, flag)
    }

    /// Open a session, uploading the agent once when it is missing or is not
    /// the build machine-service holds (GS-3).
    fn open_session(
        &self,
        vm: &str,
        target: &GuestTarget<'_>,
        flag: &Arc<AtomicBool>,
    ) -> OpResult<LiveSession> {
        let bytes = self.host.session_agent(target.kind)?;
        let build = hex(&Sha256::digest(&bytes));
        let mut reason = String::new();
        for attempt in 0..2 {
            if flag.load(Ordering::SeqCst) {
                return Err(OpError::new(format!("{vm} is being released")));
            }
            let channel = self.host.open_guest_channel(
                target.ip,
                target.user,
                target.key_dir,
                START_COMMAND,
            )?;
            let GuestChannel {
                reader,
                writer,
                mut process,
            } = channel;
            match Session::open(reader, writer, OPEN_TIMEOUT) {
                Ok(mut session) => {
                    let hello = session.hello().clone();
                    if hello.protocol == PROTOCOL && hello.build == build {
                        session.cancel_on(Arc::clone(flag));
                        if let Some(abort) = process.abort_handle() {
                            session.on_silence(abort);
                        }
                        self.log(&format!(
                            "{vm}: guest session opened (agent {}, {})",
                            &build[..12],
                            hello.os
                        ));
                        return Ok(LiveSession { session, process });
                    }
                    reason = format!(
                        "the guest runs agent build {} protocol {}, not {} protocol {PROTOCOL}",
                        hello.build, hello.protocol, build
                    );
                    LiveSession { session, process }.close();
                }
                Err(error) => {
                    process.finish(CLOSE_GRACE);
                    let diagnostics = process.diagnostics();
                    reason = if diagnostics.is_empty() {
                        error.to_string()
                    } else {
                        format!("{error}; {diagnostics}")
                    };
                }
            }
            if attempt == 0 {
                self.log(&format!(
                    "{vm}: uploading the guest session agent ({reason})"
                ));
                let uploaded = crate::proc::with_cancellation(Arc::clone(flag), || {
                    self.host.ssh(
                        target.ip,
                        target.user,
                        target.key_dir,
                        UPLOAD_COMMAND,
                        Some(bytes.clone()),
                        UPLOAD_TIMEOUT_S,
                    )
                });
                match uploaded {
                    Some((0, _)) => {}
                    other => {
                        return Err(OpError::new(format!(
                            "guest session agent upload failed: {}",
                            other
                                .map(|(rc, text)| format!("rc={rc} {text}"))
                                .unwrap_or_default()
                        )))
                    }
                }
            }
        }
        Err(OpError::new(format!(
            "guest session agent did not start: {reason}"
        )))
    }
}

/// Spawn the `ssh` channel for a lease identity.
pub fn ssh_channel(ip: &str, user: &str, key_dir: &Path, command: &str) -> OpResult<GuestChannel> {
    let argv = crate::ssh::ssh_argv(ip, user, key_dir, command)?;
    let mut process = service_layout::tools::command_for(&argv[0]).map_err(OpError::new)?;
    process.args(&argv[1..]);
    ChildChannel::spawn(process)
}
