//! How a test starts a child (DC-251, DC-256): from an empty
//! environment, in a process group of its own, with a lifeline, and ended
//! with everything related to it when its handle drops.
//!
//! Only the test builds' dev-dependencies compile this module in.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use super::processes::{self, lifeline_value};
use super::LIFELINE_VARIABLE;

/// The `PATH` every test child starts with: the system's own folders, for
/// the shell utilities a fake tool script uses. No tool the service runs is
/// found through it ([`crate::tools`]).
pub const BASE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A pipe whose read end every child started on it inherits. A clone
/// shares it: a fixture whose daemon restarts starts each daemon on one
/// lifeline, so what the first started is the second's too. When the last
/// clone and the last handle on it drop, every process still holding it is
/// ended and the pipe closes, which ends the rest through their watchers.
#[derive(Clone)]
pub struct Lifeline(std::sync::Arc<Pipe>);

struct Pipe {
    write: std::sync::Mutex<Option<OwnedFd>>,
    read: std::sync::Mutex<Option<OwnedFd>>,
    fd: i32,
    handle: u64,
}

impl Lifeline {
    /// A new lifeline.
    pub fn new() -> Lifeline {
        let (read, write) = io::pipe().expect("a lifeline's pipe");
        let read: OwnedFd = read.into();
        let fd = read.as_raw_fd();
        let handle = processes::pipe_handle(std::process::id() as i32, fd)
            .expect("the lifeline's pipe has a handle");
        Lifeline(std::sync::Arc::new(Pipe {
            write: std::sync::Mutex::new(Some(write.into())),
            read: std::sync::Mutex::new(Some(read)),
            fd,
            handle,
        }))
    }

    /// Close both ends in this process as a test that dies does, without
    /// ending anything itself: what holds the lifeline sees it end and ends
    /// its own, which a guard test proves (DC-256). Nothing can start
    /// on it afterwards.
    pub fn sever(&self) {
        for end in [&self.0.write, &self.0.read] {
            end.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
    }

    fn fd(&self) -> i32 {
        self.0.fd
    }

    fn handle(&self) -> u64 {
        self.0.handle
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // Twice: a process that forks while the first pass runs is caught by
        // the second.
        for _ in 0..2 {
            processes::kill(&processes::holders(self.fd, self.handle));
        }
    }
}

impl Default for Lifeline {
    fn default() -> Lifeline {
        Lifeline::new()
    }
}

/// A command for a test child, which derefs to its [`Command`].
pub struct Isolated {
    command: Command,
    lifeline: Lifeline,
}

/// A command for `program` with an empty environment except
/// [`BASE_PATH`] and the lifeline, no standard input, and a process group
/// of its own.
pub fn command(program: impl AsRef<OsStr>) -> Isolated {
    command_on(program, &Lifeline::new())
}

/// [`command`] on a shared lifeline.
pub fn command_on(program: impl AsRef<OsStr>, lifeline: &Lifeline) -> Isolated {
    let lifeline = lifeline.clone();
    let mut command = Command::new(program);
    let fd = lifeline.fd();
    command
        .env_clear()
        .env("PATH", BASE_PATH)
        .env(LIFELINE_VARIABLE, lifeline_value(fd, lifeline.handle()))
        .stdin(Stdio::null())
        .process_group(0);
    // SAFETY: `fcntl`, `getdtablesize` and `close` are async-signal-safe and
    // touch only the child's own descriptor table.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            // A descriptor another thread had open without `FD_CLOEXEC` at
            // the fork, such as a lifeline between its `pipe` and its
            // `FD_CLOEXEC`, never reaches the child: only its standard
            // streams and its own lifeline do.
            // Bounded, so the child, a copy of the test until its `exec`,
            // stays one only briefly.
            for other in 3..libc::getdtablesize().min(4096) {
                if other != fd {
                    let flags = libc::fcntl(other, libc::F_GETFD);
                    if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                        libc::close(other);
                    }
                }
            }
            Ok(())
        });
    }
    Isolated { command, lifeline }
}

/// The [`Command`] builder methods, each answering the isolated command, so
/// a chain always ends in [`Isolated::spawn`], [`Isolated::output`] or
/// [`Isolated::status`], which keep the lifeline with the child: a plain
/// [`Command::spawn`] would leave the child without one, and its lifeline's
/// end would end it at once.
impl Isolated {
    /// [`Command::arg`].
    pub fn arg(&mut self, argument: impl AsRef<OsStr>) -> &mut Isolated {
        self.command.arg(argument);
        self
    }

    /// [`Command::args`].
    pub fn args<I, S>(&mut self, arguments: I) -> &mut Isolated
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(arguments);
        self
    }

    /// [`Command::env`].
    pub fn env(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Isolated {
        self.command.env(name, value);
        self
    }

    /// [`Command::envs`].
    pub fn envs<I, K, V>(&mut self, variables: I) -> &mut Isolated
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.envs(variables);
        self
    }

    /// [`Command::env_remove`].
    pub fn env_remove(&mut self, name: impl AsRef<OsStr>) -> &mut Isolated {
        self.command.env_remove(name);
        self
    }

    /// [`Command::current_dir`].
    pub fn current_dir(&mut self, folder: impl AsRef<std::path::Path>) -> &mut Isolated {
        self.command.current_dir(folder);
        self
    }

    /// [`Command::stdin`].
    pub fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Isolated {
        self.command.stdin(stdio);
        self
    }

    /// [`Command::stdout`].
    pub fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Isolated {
        self.command.stdout(stdio);
        self
    }

    /// [`Command::stderr`].
    pub fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Isolated {
        self.command.stderr(stdio);
        self
    }

    /// Start the child, which keeps a share of the lifeline.
    pub fn spawn(&mut self) -> io::Result<Owned> {
        let child = self.command.spawn()?;
        Ok(Owned {
            pid: child.id() as i32,
            child: Some(child),
            lifeline: Some(self.lifeline.clone()),
        })
    }

    /// Run the child to its end with its output captured, then end what it
    /// left behind.
    pub fn output(&mut self) -> io::Result<Output> {
        self.command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut owned = self.spawn()?;
        let child = owned.child.take().expect("a started child");
        let output = child.wait_with_output();
        owned.end();
        output
    }

    /// Run the child to its end, then end what it left behind.
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        let mut owned = self.spawn()?;
        let status = owned.wait();
        owned.end();
        status
    }
}

/// A running test child; dropping it ends the child and everything related
/// to it.
pub struct Owned {
    pid: i32,
    child: Option<Child>,
    lifeline: Option<Lifeline>,
}

impl Owned {
    /// The child's process ID, which is its process group's too.
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    /// The child.
    pub fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("a running child")
    }

    /// Whether it has exited, without waiting.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Err(io::Error::other("the child was already ended")),
        }
    }

    /// Wait for it to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => child.wait(),
            None => Err(io::Error::other("the child was already ended")),
        }
    }

    /// Send `signal` to the child alone and wait up to `grace` for it to
    /// exit; what it started keeps running until this handle ends it.
    pub fn signal(&mut self, signal: i32, grace: Duration) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        // SAFETY: the PID is this handle's own unreaped child.
        unsafe { libc::kill(self.pid, signal) };
        let deadline = Instant::now() + grace;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `SIGTERM` the child, up to `grace` for it to exit, then end it and
    /// everything related to it.
    pub fn terminate(&mut self, grace: Duration) {
        let before = processes::related(self.pid, None);
        self.signal(libc::SIGTERM, grace);
        processes::kill(&before);
        self.end();
    }

    /// End the child, its process group, its descendants and the groups
    /// they lead with `SIGKILL`, and reap it. Its lifeline goes with it; the
    /// last handle on a lifeline ends every process still holding it
    /// ([`Lifeline`]).
    pub fn end(&mut self) {
        // Twice: a process that forks while the first pass runs is caught by
        // the second.
        for _ in 0..2 {
            processes::kill(&processes::related(self.pid, None));
        }
        self.lifeline.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.end();
    }
}

impl Owned {
    /// The processes related to the child now: itself, its group, its
    /// descendants and every holder of its lifeline.
    pub fn related(&self) -> Vec<processes::Row> {
        processes::related(self.pid, self.line())
    }

    fn line(&self) -> Option<(i32, u64)> {
        self.lifeline
            .as_ref()
            .map(|line| (line.fd(), line.handle()))
    }
}

/// The rows of `rows` that still name a living process.
pub fn living(rows: &[processes::Row]) -> Vec<processes::Row> {
    let table = processes::table();
    rows.iter()
        .filter(|row| {
            table
                .iter()
                .any(|now| now.pid == row.pid && now.start == row.start)
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child holds its own lifeline and none of the test's other
    /// lifelines, and the test that owns them is never their holder.
    #[test]
    fn a_child_holds_only_its_own_lifeline() {
        let other = Lifeline::new();
        let mine = Lifeline::new();
        let mut command = command_on("/bin/sleep", &mine);
        command.arg("5");
        let child = command.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let pid = child.id() as i32;
        assert_eq!(processes::pipe_handle(pid, mine.fd()), Some(mine.handle()));
        assert_eq!(processes::pipe_handle(pid, other.fd()), None);
        let holders = processes::holders(mine.fd(), mine.handle());
        assert_eq!(
            holders.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![pid]
        );
    }

    /// Children started at once from many threads hold only their own
    /// lifelines, so ending one never ends another.
    #[test]
    fn concurrent_children_are_never_ended_by_each_other() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..3)
                        .map(|_| {
                            let mut command = command("/bin/sh");
                            command.args(["-c", "sleep 0.2"]);
                            command.output().unwrap().status.code()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), vec![Some(0); 3]);
        }
    }
}
