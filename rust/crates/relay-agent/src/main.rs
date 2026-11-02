//! `relay-agent`: the relay agent daemon (`docs/arch/relay-agent.md`).
//!
//! - `relay-agent run` waits for `~/.config/relay-agent/config.json`, then
//!   dials the broker and serves it, as launchd or systemd starts it.
//! - `relay-agent stdio` serves one link of length-prefixed messages on
//!   standard input and output, and stops when the input ends.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use relay_agent::config::{agent_root, config_path, load, wait_for};
use relay_agent::dial::run;
use relay_agent::session_link::{self, DialOptions};
use relay_agent::{Agent, AgentOptions, StreamLink};

/// How long a stopping agent waits for running commands to record their end.
const GRACE: Duration = Duration::from_secs(2);
/// How often the agent looks for its configuration.
const CONFIG_POLL: Duration = Duration::from_secs(1);

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["run"] => serve_broker(),
        ["stdio"] => serve_stdio(),
        ["--version"] => println!("relay-agent {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("usage: relay-agent run | relay-agent stdio | relay-agent --version");
            std::process::exit(2);
        }
    }
}

fn open_agent() -> Agent {
    let options = AgentOptions::from_environment();
    eprintln!(
        "relay-agent: commands are owned by {}",
        options.ownership.describe()
    );
    match Agent::open(options.clone()) {
        Ok(agent) => {
            // A restart executes the agent's path again, so a build renamed
            // into place there runs in this same process (RA-6).
            let executable = options.executable.clone();
            agent.set_restart(move || {
                eprintln!("relay-agent: restarting onto {}", executable.display());
                relay_agent::agent::reexec(&executable)
            });
            agent
        }
        Err(error) => {
            eprintln!(
                "relay-agent: cannot open its records in {}: {error}",
                options.state.display()
            );
            std::process::exit(1);
        }
    }
}

fn serve_broker() {
    let running = stop_on_signal();
    let path = config_path(&agent_root());
    let config = wait_for(&path, CONFIG_POLL, |reason| {
        eprintln!("relay-agent: waiting for its configuration: {reason}");
    });
    eprintln!(
        "relay-agent: configuration loaded; broker {}",
        config.broker_url
    );
    let options = match DialOptions::from_environment() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("relay-agent: {error}");
            std::process::exit(2);
        }
    };
    let agent = open_agent();
    let _ = running.set(agent.clone());
    // The file is read again before every dial, so a rotated credential or a
    // moved broker takes effect without a restart; a file that stops loading
    // leaves the last good configuration in use.
    let mut current = config;
    run(
        &agent,
        &current.clone(),
        |_| {
            if let Ok(fresh) = load(&path) {
                if fresh != current {
                    eprintln!(
                        "relay-agent: configuration changed; broker {}",
                        fresh.broker_url
                    );
                    current = fresh;
                }
            }
            session_link::dial(&current, &options)
        },
        |reason| eprintln!("relay-agent: {reason}"),
    );
}

fn serve_stdio() {
    let running = stop_on_signal();
    let agent = open_agent();
    let _ = running.set(agent.clone());
    // SAFETY: descriptors 0 and 1 are open for the life of the process and
    // nothing else in this program uses them. Raw files avoid the line
    // buffering of `Stdout`.
    let (input, output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let link = StreamLink::new(
        BufReader::with_capacity(1 << 20, input),
        BufWriter::with_capacity(1 << 20, output),
    );
    let served = agent.serve(Arc::new(link));
    agent.shutdown(GRACE);
    match served {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("relay-agent: {error}");
            std::process::exit(1);
        }
    }
}

/// The write end of the pipe the signal handler reports stop signals on.
static SIGNALS: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_stop_signal(signal: libc::c_int) {
    let byte = signal as u8;
    // SAFETY: `write` is async-signal-safe, and the descriptor stays open
    // for the life of the process.
    unsafe {
        libc::write(
            SIGNALS.load(Ordering::Relaxed),
            (&byte as *const u8).cast(),
            1,
        );
    }
}

/// Catch the stop signals and, on one, kill the process group of every
/// command that the agent put in the returned slot is running, record it,
/// and exit. A caught signal returns to its default action in a command,
/// because `exec` resets handlers, while a blocked signal would stay blocked
/// in every command; so the agent catches the signals and never blocks them.
fn stop_on_signal() -> Arc<OnceLock<Agent>> {
    let mut pipe = [0; 2];
    // SAFETY: `pipe` writes two descriptors into the array; `sigaction`
    // installs a handler that only calls `write`; the descriptors are marked
    // close-on-exec so commands do not inherit them.
    unsafe {
        if libc::pipe(pipe.as_mut_ptr()) != 0 {
            eprintln!("relay-agent: cannot create the signal pipe");
            std::process::exit(1);
        }
        for descriptor in pipe {
            libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        SIGNALS.store(pipe[1], Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    // SAFETY: the read end is open and owned only by this file from here on.
    let mut signals = unsafe { File::from_raw_fd(pipe[0]) };
    let slot: Arc<OnceLock<Agent>> = Arc::default();
    let running = Arc::clone(&slot);
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        let signal = loop {
            match signals.read(&mut byte) {
                Ok(1) => break i32::from(byte[0]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return,
            }
        };
        if let Some(agent) = running.get() {
            agent.shutdown(GRACE);
        }
        std::process::exit(128 + signal);
    });
    slot
}
