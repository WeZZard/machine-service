//! The rig's child processes.
//!
//! Each child runs in its own process group with its output in a log file of
//! the run directory, and dropping it stops the whole group: mitmproxy's Linux
//! build is a self-unpacking executable that runs the real program as its own
//! child, which a signal to the first process alone would leave running.

use std::fs::File;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, Command, Stdio};
use std::time::{Duration, Instant};

use crate::RigError;

/// A running child process of the rig.
#[derive(Debug)]
pub struct Child {
    name: String,
    child: StdChild,
    log: PathBuf,
}

impl Child {
    /// Start `command` as `name`, with standard output and error in
    /// `<run_dir>/<name>.log`.
    pub fn spawn(name: &str, mut command: Command, run_dir: &Path) -> Result<Child, RigError> {
        let log = run_dir.join(format!("{name}.log"));
        let out = File::create(&log).map_err(RigError::io(format!("create {}", log.display())))?;
        let err = out
            .try_clone()
            .map_err(RigError::io("duplicate the log file"))?;
        let child = command
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0)
            .spawn()
            .map_err(|error| RigError::Process {
                name: name.into(),
                reason: error.to_string(),
            })?;
        Ok(Child {
            name: name.into(),
            child,
            log,
        })
    }

    /// The child's log file.
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// Wait until `ready` holds, failing if the child exits or `timeout` passes.
    pub fn wait_until(
        &mut self,
        timeout: Duration,
        mut ready: impl FnMut() -> bool,
    ) -> Result<(), RigError> {
        let deadline = Instant::now() + timeout;
        loop {
            if ready() {
                return Ok(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(self.failure(format!("exited with {status} before it was ready")));
            }
            if Instant::now() >= deadline {
                return Err(self.failure(format!("not ready after {} s", timeout.as_secs())));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn failure(&self, reason: String) -> RigError {
        let tail = std::fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = tail.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        RigError::Process {
            name: self.name.clone(),
            reason: format!("{reason}\n{}", tail.join("\n")),
        }
    }

    fn signal_group(&self, signal: libc::c_int) {
        let group = self.child.id() as libc::pid_t;
        // SAFETY: kill(2) on the child's own process group; no memory is shared.
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                // The group may still hold the real program of a self-unpacking build.
                self.signal_group(libc::SIGKILL);
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.signal_group(libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// A loopback port that was free a moment ago.
pub fn free_port() -> Result<u16, RigError> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(RigError::io("find a free port"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(RigError::io("read the free port"))
}

/// Whether something accepts connections at `addr`.
pub fn accepts(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}
