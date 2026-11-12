//! A command as a launchd job of its own (RA-10, DC-310, DC-312, DC-314).
//!
//! On macOS a relayed command is never a child of the relay agent: macOS
//! charges a privacy request to the first program that launchd starts for
//! a job, so the job's first program is the user's shell, and nothing that
//! the agent holds is inherited.
//!
//! - One FIFO set per job, mode 600 in a mode-700 folder: `env`, `in`, `out`
//!   and `status`. The reader opens `out` and `status` for reading and
//!   writing, and `in` for writing, before the job is registered, because
//!   launchd opens a job's standard input and output when it starts it and
//!   opening a FIFO waits for its peer; holding a writer end also means a
//!   read never sees a false end of file while a background process keeps
//!   `out` open.
//! - The shell runs [`PRELUDE`]: it reports its process group on `status`,
//!   reads `env` once with `cat` and evaluates it (the environment, the
//!   working directory and the command; bash's `.` reads nothing from a
//!   FIFO, whose size is 0), runs the command as `$SHELL -c <command>`, its own child,
//!   named by the shell's file name, and reports the command's exit status
//!   on `status`. Neither the job's plist nor `launchctl print` holds a
//!   value from `env`.
//! - `launchctl print` is not an interface (`man launchctl`), so the status
//!   is the prelude's; `print` is read only for a job that ended without
//!   writing one, as a best effort.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use service_layout::tools::{self, Tool};

/// The prefix of every job label the relay agent makes.
pub const LABEL_PREFIX: &str = "com.wezzard.relay-agent.job.";

/// How long the shell may take to open `env` before the job is stopped.
pub const ENV_DEADLINE: Duration = Duration::from_secs(10);

/// The exit status the prelude reports when `env` could not be read or
/// the working directory could not be entered.
pub const PRELUDE_FAILED: i32 = 125;

/// The script the job's shell runs: `$0` is the `env` FIFO and `$1` the
/// `status` FIFO. It reports its group, sources `env`, runs the command as
/// the shell's own child and reports its exit status. The reports go to
/// descriptor 9, which the command never inherits: the subshell closes it
/// itself, because a redirection on the subshell would have the shell save
/// it to a higher descriptor that the command keeps.
pub const PRELUDE: &str = r#"exec 9>"$1"
printf 'group %s\n' "$$" >&9
if ! eval "$(/bin/cat "$0")"; then printf 'exit 125\n' >&9; exit 125; fi
c=$RELAY_JOB_COMMAND s=$RELAY_JOB_SHELL n=$RELAY_JOB_NAME
unset RELAY_JOB_COMMAND RELAY_JOB_SHELL RELAY_JOB_NAME
(exec 9>&-; exec -a "$n" "$s" -c "$c")
r=$?
printf 'exit %s\n' "$r" >&9
exit "$r"
"#;

/// The launchd that runs jobs in this user's GUI domain.
#[derive(Clone, Debug)]
pub struct Launchd {
    program: PathBuf,
    domain: String,
}

impl Launchd {
    /// The system `launchctl` and `gui/<uid>`; in a test build, the test's
    /// own `launchctl` from its tool table, never the system one (DC-254).
    pub fn resolve() -> Result<Launchd, String> {
        // SAFETY: `getuid` only reads process state.
        let uid = unsafe { libc::getuid() };
        Ok(Launchd {
            program: tools::path(Tool::Launchctl)?,
            domain: format!("gui/{uid}"),
        })
    }

    fn run(&self, arguments: &[&std::ffi::OsStr]) -> io::Result<std::process::Output> {
        Command::new(&self.program)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
    }

    /// Register and start the job that `plist` describes.
    pub fn bootstrap(&self, plist: &Path) -> io::Result<()> {
        let output = self.run(&[
            "bootstrap".as_ref(),
            self.domain.as_ref(),
            plist.as_os_str(),
        ])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "launchctl bootstrap {}: {}",
                self.domain,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    /// Stop the job `label` and every process left in its group, and
    /// remove it. A job that is gone already is not an error.
    pub fn bootout(&self, label: &str) {
        let target = format!("{}/{label}", self.domain);
        let _ = self.run(&["bootout".as_ref(), target.as_ref()]);
    }

    /// Whether the job `label` is registered and still running.
    pub fn is_running(&self, label: &str) -> bool {
        self.print(label)
            .is_some_and(|text| text.lines().any(|line| line.trim() == "state = running"))
    }

    /// The job's last exit status or terminating signal, from `launchctl
    /// print`, as a best effort for a job that ended without reporting one;
    /// the output is not an interface (`man launchctl`).
    pub fn last_end(&self, label: &str) -> (Option<i32>, Option<i32>) {
        let Some(text) = self.print(label) else {
            return (None, None);
        };
        let mut rc = None;
        let mut signal = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("last exit code = ") {
                rc = value.trim().parse().ok();
            } else if let Some(value) = line.strip_prefix("last terminating signal = ") {
                signal = value
                    .rsplit(':')
                    .next()
                    .and_then(|number| number.trim().parse().ok());
            }
        }
        (rc, signal)
    }

    fn print(&self, label: &str) -> Option<String> {
        let target = format!("{}/{label}", self.domain);
        let output = self.run(&["print".as_ref(), target.as_ref()]).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// A job label, unique on this machine: the agent's prefix, the hash of
/// the execution ID and a suffix that no other job of this agent shares.
pub fn label_for(agent: &str, key: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!(
        "{LABEL_PREFIX}{agent}.{key}.{millis}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// One job's FIFOs and plist, in a folder of their own.
#[derive(Clone, Debug)]
pub struct JobFiles {
    /// The folder, mode 700.
    pub dir: PathBuf,
    /// The environment, working directory and command, read once.
    pub env: PathBuf,
    /// Standard input.
    pub input: PathBuf,
    /// Standard output and standard error.
    pub out: PathBuf,
    /// The prelude's reports: `group <n>` and `exit <n>`.
    pub status: PathBuf,
    /// The job's own output when another process redacts it into `out`
    /// (DM-4); a FIFO only then.
    pub raw: PathBuf,
    /// The job's plist.
    pub plist: PathBuf,
}

impl JobFiles {
    /// Make the folder `dir` and its FIFOs. `dir` must not exist.
    pub fn create(dir: &Path) -> io::Result<JobFiles> {
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
        let files = JobFiles::at(dir);
        for fifo in [&files.env, &files.input, &files.out, &files.status] {
            make_fifo(fifo)?;
        }
        Ok(files)
    }

    /// The paths of the FIFO set in `dir`, made or not.
    pub fn at(dir: &Path) -> JobFiles {
        JobFiles {
            dir: dir.to_path_buf(),
            env: dir.join("env"),
            input: dir.join("in"),
            out: dir.join("out"),
            status: dir.join("status"),
            raw: dir.join("raw"),
            plist: dir.join("job.plist"),
        }
    }

    /// Remove the folder and everything in it.
    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Make the FIFO `path`, mode 600.
pub fn make_fifo(path: &Path) -> io::Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a FIFO path holds a NUL byte"))?;
    // SAFETY: `path` is a valid NUL-terminated string.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The plist of a job that runs [`PRELUDE`] in `shell`, its output going
/// to `out`, which is `files.out` unless another process redacts it.
pub fn plist(label: &str, shell: &Path, files: &JobFiles, stdin: bool, out: &Path) -> Vec<u8> {
    let arguments = [
        shell.as_os_str(),
        "-c".as_ref(),
        PRELUDE.as_ref(),
        files.env.as_os_str(),
        files.status.as_os_str(),
    ];
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n",
    );
    body.push_str(&format!(
        "<key>Label</key><string>{}</string>\n",
        xml(label)
    ));
    body.push_str("<key>ProgramArguments</key><array>\n");
    for argument in arguments {
        body.push_str(&format!(
            "<string>{}</string>\n",
            xml(&argument.to_string_lossy())
        ));
    }
    body.push_str("</array>\n");
    let out = xml(&out.to_string_lossy());
    body.push_str(&format!(
        "<key>StandardOutPath</key><string>{out}</string>\n\
         <key>StandardErrorPath</key><string>{out}</string>\n"
    ));
    if stdin {
        body.push_str(&format!(
            "<key>StandardInPath</key><string>{}</string>\n",
            xml(&files.input.to_string_lossy())
        ));
    }
    // A command's background processes outlive it, as they did before; the
    // job's stop kills its group itself (DC-316).
    body.push_str("<key>AbandonProcessGroup</key><true/>\n");
    body.push_str("<key>RunAtLoad</key><true/>\n</dict></plist>\n");
    body.into_bytes()
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// What the `env` FIFO holds: an `export` for each variable whose name a
/// shell accepts, the working directory, and the command, each value
/// quoted for the shell.
pub fn env_script(
    environment: &[(OsString, OsString)],
    cwd: &Path,
    shell: &Path,
    command: &str,
) -> Vec<u8> {
    let mut script = Vec::new();
    for (name, value) in environment {
        let name = name.as_bytes();
        let valid = !name.is_empty()
            && !name[0].is_ascii_digit()
            && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
            && !name.starts_with(b"RELAY_JOB_");
        if !valid {
            continue;
        }
        script.extend_from_slice(b"export ");
        script.extend_from_slice(name);
        script.push(b'=');
        script.extend_from_slice(&quote(value.as_bytes()));
        script.push(b'\n');
    }
    script.extend_from_slice(b"cd -- ");
    script.extend_from_slice(&quote(cwd.as_os_str().as_bytes()));
    script.extend_from_slice(b" || return 125\n");
    let name = shell
        .file_name()
        .map(|name| name.as_bytes().to_vec())
        .unwrap_or_else(|| shell.as_os_str().as_bytes().to_vec());
    for (variable, value) in [
        ("RELAY_JOB_SHELL", shell.as_os_str().as_bytes().to_vec()),
        ("RELAY_JOB_NAME", name),
        ("RELAY_JOB_COMMAND", command.as_bytes().to_vec()),
    ] {
        script.extend_from_slice(variable.as_bytes());
        script.push(b'=');
        script.extend_from_slice(&quote(&value));
        script.push(b'\n');
    }
    script
}

/// `bytes` in single quotes, each `'` written as `'\''`.
fn quote(bytes: &[u8]) -> Vec<u8> {
    let mut quoted = vec![b'\''];
    for &byte in bytes {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

/// Write `script` into the `env` FIFO once the job's shell opens it, or
/// fail after `deadline`, so an agent never waits on a shell that never
/// started.
pub fn write_env(env: &Path, script: &[u8], deadline: Duration) -> io::Result<()> {
    let started = Instant::now();
    let mut file = loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(env)
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                if started.elapsed() > deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "the job's shell did not open its environment within {} s",
                            deadline.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
    set_blocking(&file, true)?;
    file.write_all(script)
}

fn set_blocking(file: &File, blocking: bool) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fcntl` on a descriptor this function borrows.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if blocking {
        flags & !libc::O_NONBLOCK
    } else {
        flags | libc::O_NONBLOCK
    };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open a FIFO for reading and writing without blocking.
pub fn open_held(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// A started job, its reader's ends, and what stops it.
pub struct Job {
    /// The launchd that runs it.
    pub launchd: Launchd,
    /// Its label.
    pub label: String,
    /// Its files.
    pub files: JobFiles,
    /// The job's process group, which its shell leads, as the prelude
    /// reported it; 0 when it never did.
    pub group: i32,
    pub(crate) out: File,
    pub(crate) status: File,
    pub(crate) input: Option<File>,
    pub(crate) reports: Reports,
}

/// How long the prelude may take to report its group.
pub const GROUP_DEADLINE: Duration = Duration::from_secs(10);

/// Which side writes the job's `env`.
pub enum EnvWriter<'a> {
    /// This process, with these variables and this working directory.
    Here {
        /// The variables.
        environment: &'a [(OsString, OsString)],
        /// The working directory.
        cwd: &'a Path,
    },
    /// Another process that made the FIFOs, such as machine-service on a
    /// durable Mac (DC-314), writes it; this side never sees it. When
    /// `redacted`, the job writes to `raw`, and that process writes the
    /// redacted output to `out`.
    Elsewhere {
        /// Whether the job's output goes through `raw`.
        redacted: bool,
    },
}

/// Start `command` as the job `label`: make or take the FIFO set in `dir`,
/// open its reader ends, register the job, and write its `env` when this
/// side does.
pub fn start(
    launchd: &Launchd,
    label: &str,
    shell: &Path,
    dir: &Path,
    command: &str,
    stdin: bool,
    writer: EnvWriter<'_>,
) -> io::Result<Job> {
    let files = match writer {
        EnvWriter::Here { .. } => JobFiles::create(dir)?,
        EnvWriter::Elsewhere { .. } => JobFiles::at(dir),
    };
    let opened = (|| {
        let out = open_held(&files.out)?;
        let status = open_held(&files.status)?;
        let input = if stdin {
            Some(open_held(&files.input)?)
        } else {
            None
        };
        let job_out = match writer {
            EnvWriter::Elsewhere { redacted: true } => &files.raw,
            _ => &files.out,
        };
        std::fs::write(&files.plist, plist(label, shell, &files, stdin, job_out))?;
        Ok::<_, io::Error>((out, status, input))
    })();
    let (out, status, input) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            if matches!(writer, EnvWriter::Here { .. }) {
                files.remove();
            }
            return Err(error);
        }
    };
    if let Err(error) = launchd.bootstrap(&files.plist) {
        if matches!(writer, EnvWriter::Here { .. }) {
            files.remove();
        }
        return Err(error);
    }
    if let EnvWriter::Here { environment, cwd } = writer {
        let script = env_script(environment, cwd, shell, command);
        if let Err(error) = write_env(&files.env, &script, ENV_DEADLINE) {
            launchd.bootout(label);
            files.remove();
            return Err(error);
        }
    }
    // The prelude reports its group before it reads `env`, so the group is
    // known by now, or soon: wait for it, so that a stop can kill it.
    let mut status = status;
    let mut reports = Reports::default();
    let mut buffer = [0u8; 256];
    let started = Instant::now();
    while reports.group.is_none() && reports.exit.is_none() && started.elapsed() < GROUP_DEADLINE {
        drain(&mut status, &mut buffer, |bytes| reports.take(bytes));
        if reports.group.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(Job {
        launchd: launchd.clone(),
        label: label.to_string(),
        files,
        group: reports.group.unwrap_or(0),
        out,
        status,
        input,
        reports,
    })
}

/// Stop a job: kill its process group, which `AbandonProcessGroup` leaves
/// to its owner, then remove the job, which on its own ends only its shell
/// (DC-316).
pub fn stop(launchd: &Launchd, label: &str, group: i32) {
    if group > 1 {
        crate::kill_group(group);
    }
    launchd.bootout(label);
}

/// What the prelude reported on `status`.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Reports {
    pub group: Option<i32>,
    pub exit: Option<i32>,
    partial: Vec<u8>,
}

impl Reports {
    /// Take the lines of `bytes`.
    pub(crate) fn take(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if let Some(value) = line.strip_prefix("group ") {
                self.group = value.trim().parse().ok();
            } else if let Some(value) = line.strip_prefix("exit ") {
                self.exit = value.trim().parse().ok();
            }
        }
    }
}

/// The exit status and signal a shell's `$?` stands for: 128 plus a
/// signal's number reads as that signal, as a shell reports it.
pub(crate) fn exit_of(status: i32) -> (Option<i32>, Option<i32>) {
    if (129..=128 + 64).contains(&status) {
        (None, Some(status - 128))
    } else {
        (Some(status), None)
    }
}

/// Read whatever `file` holds now, without waiting.
pub(crate) fn drain(file: &mut File, buffer: &mut [u8], mut sink: impl FnMut(&[u8])) {
    loop {
        match file.read(buffer) {
            Ok(0) => return,
            Ok(count) => sink(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Wait until `out` or `status` has bytes, or `timeout` passes.
pub(crate) fn wait_readable(out: &File, status: &File, timeout: Duration) {
    let mut fds = [
        libc::pollfd {
            fd: out.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: status.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fds` is a valid array of two pollfd structures.
    unsafe { libc::poll(fds.as_mut_ptr(), 2, millis) };
}

/// The bytes of `path` as an [`OsString`].
pub fn os(path: &Path) -> OsString {
    OsString::from_vec(path.as_os_str().as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{spawn_job, Exit};
    use std::sync::Once;

    /// Point this test process's tool table at a folder holding only the
    /// fake `launchctl`, and the fake's state at a folder of its own.
    fn fake_launchd() -> Launchd {
        static SETUP: Once = Once::new();
        SETUP.call_once(|| {
            let root =
                std::env::temp_dir().join(format!("guest-actions-launchd-{}", std::process::id()));
            let tools = root.join("tools");
            std::fs::create_dir_all(&tools).unwrap();
            let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../service-layout/tests/fixtures/launchd/launchctl")
                .canonicalize()
                .unwrap();
            let _ = std::os::unix::fs::symlink(&fake, tools.join("launchctl"));
            // SAFETY: set once, before any test of this module reads them.
            unsafe {
                std::env::set_var(tools::TEST_VARIABLE, &tools);
                std::env::set_var("FAKE_LAUNCHD_STATE", root.join("state"));
            }
        });
        Launchd::resolve().unwrap()
    }

    fn run(command: &str, stdin: Option<&[u8]>, timeout: Duration) -> (Exit, String, Duration) {
        let launchd = fake_launchd();
        let base = tempfile::tempdir().unwrap();
        let label = label_for("test", "k");
        let environment = vec![(OsString::from("JOBTEST"), OsString::from("from-env"))];
        let spawned = spawn_job(
            &launchd,
            &label,
            Path::new("/bin/zsh"),
            &base.path().join("job"),
            command,
            stdin.is_some(),
            EnvWriter::Here {
                environment: &environment,
                cwd: base.path(),
            },
        )
        .unwrap();
        let started = Instant::now();
        let mut output = Vec::new();
        let input =
            stdin.map(|bytes| Box::new(io::Cursor::new(bytes.to_vec())) as Box<dyn Read + Send>);
        let exit = spawned.supervise_from(input, timeout, |bytes| {
            output.extend_from_slice(bytes);
            true
        });
        assert!(!base.path().join("job").exists(), "the job's files stay");
        (
            exit,
            String::from_utf8_lossy(&output).into_owned(),
            started.elapsed(),
        )
    }

    #[test]
    fn a_job_runs_its_command_through_the_shells_dash_c_with_its_environment() {
        let (exit, output, _) = run(
            r#"echo "zero=$0 var=$JOBTEST dir=$PWD"; nosuchcommand; exit 7"#,
            None,
            Duration::from_secs(20),
        );
        assert_eq!(exit.rc, Some(7), "{output}");
        assert!(output.contains("zero=zsh var=from-env"), "{output}");
        assert!(
            output.contains("zsh:1: command not found: nosuchcommand"),
            "{output}"
        );
    }

    #[test]
    fn a_job_reads_a_script_on_standard_input() {
        let (exit, output, _) = run(
            "zsh -s",
            Some(b"echo from-stdin $((6 * 7))\n"),
            Duration::from_secs(20),
        );
        assert_eq!(exit.rc, Some(0), "{output}");
        assert_eq!(output, "from-stdin 42\n");
    }

    #[test]
    fn a_signal_reads_as_the_signal_a_shell_reports() {
        let (exit, output, _) = run("kill -TERM $$", None, Duration::from_secs(20));
        assert_eq!((exit.rc, exit.signal), (None, Some(15)), "{output}");
    }

    /// The command holds no FIFO of its job beyond its standard streams,
    /// so what it leaves running keeps none of them open.
    #[test]
    fn a_command_inherits_no_descriptor_of_its_job() {
        let (exit, output, _) = run(
            "/usr/sbin/lsof -p $$ -a -d 3-99 -F tn | /usr/bin/grep -c '^tFIFO' || true",
            None,
            Duration::from_secs(20),
        );
        assert_eq!(exit.rc, Some(0), "{output}");
        assert_eq!(output.trim(), "0", "{output}");
    }

    #[test]
    fn a_timeout_boots_the_job_out() {
        let (exit, _, took) = run("sleep 60", None, Duration::from_secs(1));
        assert!(exit.timed_out);
        assert!(took < Duration::from_secs(10), "{took:?}");
    }

    #[test]
    fn a_killer_ends_a_running_job() {
        let launchd = fake_launchd();
        let base = tempfile::tempdir().unwrap();
        let label = label_for("test", "kill");
        let environment = Vec::new();
        let spawned = spawn_job(
            &launchd,
            &label,
            Path::new("/bin/zsh"),
            &base.path().join("job"),
            "sleep 60",
            false,
            EnvWriter::Here {
                environment: &environment,
                cwd: base.path(),
            },
        )
        .unwrap();
        let killer = spawned.killer();
        let started = Instant::now();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            killer.kill();
        });
        let exit = spawned.supervise_from(None, Duration::from_secs(60), |_| true);
        stopper.join().unwrap();
        assert!(!exit.timed_out);
        assert_eq!(exit.rc, None, "{exit:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the PID exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// DC-316: a process a command leaves in the background outlives the
    /// command, as a server started with `nohup … &` must.
    #[test]
    fn a_background_child_outlives_its_command() {
        let (exit, output, _) = run(
            "nohup sleep 60 >/dev/null 2>&1 & echo $!",
            None,
            Duration::from_secs(20),
        );
        assert_eq!(exit.rc, Some(0), "{output}");
        let child: i32 = output.trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let survived = alive(child);
        crate::kill_group(child);
        // SAFETY: plain signal to the test's own leftover.
        unsafe { libc::kill(child, libc::SIGKILL) };
        assert!(
            survived,
            "the background child {child} died with its command"
        );
    }

    /// DC-316: stopping a running job kills its whole process group, which
    /// `bootout` alone would not.
    #[test]
    fn a_stopped_job_ends_its_whole_tree() {
        let launchd = fake_launchd();
        let base = tempfile::tempdir().unwrap();
        let label = label_for("test", "tree");
        let environment = Vec::new();
        let pid_file = base.path().join("child.pid");
        let spawned = spawn_job(
            &launchd,
            &label,
            Path::new("/bin/zsh"),
            &base.path().join("job"),
            &format!(
                "sleep 61 & echo $! > {}; sleep 62; wait",
                pid_file.display()
            ),
            false,
            EnvWriter::Here {
                environment: &environment,
                cwd: base.path(),
            },
        )
        .unwrap();
        assert!(spawned.group > 1, "the prelude reported no group");
        let killer = spawned.killer();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            killer.kill();
        });
        let exit = spawned.supervise_from(None, Duration::from_secs(60), |_| true);
        stopper.join().unwrap();
        let child: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_gone(child),
            "the background sleep {child} survived the stop"
        );
        assert_eq!(exit.signal, Some(libc::SIGKILL), "{exit:?}");
    }

    #[test]
    fn an_environment_nobody_opens_times_out_instead_of_waiting() {
        let base = tempfile::tempdir().unwrap();
        let files = JobFiles::create(&base.path().join("job")).unwrap();
        let started = Instant::now();
        let error = write_env(&files.env, b"x", Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn the_env_script_quotes_every_value_and_skips_invalid_names() {
        let environment = vec![
            (OsString::from("A"), OsString::from("it's $HOME `x`")),
            (OsString::from("1BAD"), OsString::from("x")),
            (OsString::from("RELAY_JOB_COMMAND"), OsString::from("evil")),
        ];
        let script = env_script(
            &environment,
            Path::new("/tmp/a b"),
            Path::new("/bin/zsh"),
            "echo 'hi'",
        );
        let script = String::from_utf8(script).unwrap();
        assert!(
            script.contains("export A='it'\\''s $HOME `x`'\n"),
            "{script}"
        );
        assert!(
            !script.contains("1BAD") && !script.contains("evil"),
            "{script}"
        );
        assert!(
            script.contains("cd -- '/tmp/a b' || return 125\n"),
            "{script}"
        );
        assert!(
            script.contains("RELAY_JOB_COMMAND='echo '\\''hi'\\'''\n"),
            "{script}"
        );
    }

    #[test]
    fn reports_and_exit_statuses_parse_as_the_prelude_writes_them() {
        let mut reports = Reports::default();
        reports.take(b"group 12\nexi");
        assert_eq!((reports.group, reports.exit), (Some(12), None));
        reports.take(b"t 7\n");
        assert_eq!(reports.exit, Some(7));
        assert_eq!(exit_of(0), (Some(0), None));
        assert_eq!(exit_of(143), (None, Some(15)));
        assert_eq!(exit_of(200), (Some(200), None));
        assert!(label_for("a", "k") != label_for("a", "k"));
        assert!(label_for("a", "k").starts_with(LABEL_PREFIX));
    }
}
