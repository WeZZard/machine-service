//! `network-rig`: a chain of the TL-7 network rig as a process of its own,
//! for a relay agent's leg, which runs on the machine the agent dials from
//! rather than in the suite's process (`docs/arch/test-layers.md`, TL-7).
//!
//! ```text
//! network-rig serve --profile N1 --seed 7 --upstream 127.0.0.1:6241 \
//!     --upstream-ca ca.pem --broker-name host.example --listen 0.0.0.0:6244 --admit guests \
//!     [--edge-ca-key ca.key --edge-ca-cert ca.pem --edge-ca-name ca.name] \
//!     [--edge-host-addresses] --run-dir DIR
//! network-rig ctl --run-dir DIR '{"cut":{"side":"both"}}'
//! ```
//!
//! With `--edge-host-addresses` the edge also serves a client that dials one
//! of this machine's IPv4 addresses instead of the name, as a relay agent
//! dials its host's address on the guest network (BK-5), and starts again
//! when the addresses change, since a guest network appears with its first
//! VM. `--admit guests` closes every connection from a peer that is neither
//! on loopback nor on a bridge interface's subnet, so a chain that listens
//! on every interface for its guests never serves the LAN. `serve` starts the chain, writes `serve.json` (its port, URL and PID) into
//! the run directory, plays the profile's schedule unless `--no-play`, logs
//! each played step to `notes.jsonl`, and serves control requests on the Unix
//! socket `control.sock` there until it receives SIGTERM, SIGINT or a
//! `"shutdown"` request. `ctl` sends one request and prints the answer.
//!
//! Requests, one JSON value per line: `{"cut":{"side":"client"|"broker"|"both"}}`
//! cuts every open connection; `{"apply":<fault>}` and `{"revert":<fault>}` set
//! and clear a fault of the schedule's form; `"connections"`, `"play"`,
//! `"stop_playing"` and `"shutdown"`.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use network_rig::{EdgeAuthority, Fault, Profile, Rig, RigConfig, Select, Side};
use serde_json::{json, Value};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn usage() -> ! {
    eprintln!(
        "usage: network-rig serve --profile <N0..N3> [--profile-file <json>] [--seed <n>] \
--upstream <addr:port> --upstream-ca <pem> --broker-name <name> [--listen <addr:port> [--admit any|guests]] \
[--edge-ca-key <pem> --edge-ca-cert <pem> [--edge-ca-name <file>]] [--edge-host-addresses] \
--run-dir <dir> [--no-play]\n       \
network-rig ctl --run-dir <dir> <json request>\n       \
network-rig fetch   (into NETWORK_RIG_CACHE)"
    );
    std::process::exit(2)
}

struct Args {
    flags: Vec<(String, Option<String>)>,
    rest: Vec<String>,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Args {
        let mut flags = Vec::new();
        let mut rest = Vec::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if let Some(name) = arg.strip_prefix("--") {
                let value = match args.peek() {
                    Some(next) if !next.starts_with("--") => args.next(),
                    _ => None,
                };
                flags.push((name.to_string(), value));
            } else {
                rest.push(arg);
            }
        }
        Args { flags, rest }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| flag == name)
            .and_then(|(_, value)| value.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| flag == name)
    }

    fn need(&self, name: &str) -> &str {
        self.get(name).unwrap_or_else(|| {
            eprintln!("network-rig: --{name} is required");
            usage()
        })
    }
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| {
        eprintln!("network-rig: read {path}: {error}");
        std::process::exit(1)
    })
}

fn fail(what: impl std::fmt::Display) -> ! {
    eprintln!("network-rig: {what}");
    std::process::exit(1)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| usage());
    let args = Args::parse(args);
    match command.as_str() {
        "serve" => serve(&args),
        // Fill NETWORK_RIG_CACHE with this platform's pinned tools, checked
        // by their hashes, so a lab runner VM that reaches only GitHub
        // finds them there (docs/arch/lab-runners.md, LR-9).
        "fetch" => {
            for tool in [
                network_rig::tools::Tool::Toxiproxy,
                network_rig::tools::Tool::Mitmdump,
            ] {
                match network_rig::tools::ensure(tool) {
                    Ok(path) => println!("{}", path.display()),
                    Err(error) => fail(error),
                }
            }
        }
        "ctl" => {
            let dir = PathBuf::from(args.need("run-dir"));
            let request = args.rest.first().unwrap_or_else(|| usage());
            match send(&dir, request) {
                Ok(answer) => {
                    println!("{answer}");
                    if answer.get("error").is_some() {
                        std::process::exit(1)
                    }
                }
                Err(error) => fail(error),
            }
        }
        _ => usage(),
    }
}

/// Send one request to the chain served from `run_dir`.
fn send(run_dir: &Path, request: &str) -> Result<Value, String> {
    let socket = run_dir.join("control.sock");
    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| format!("connect {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    writeln!(stream, "{}", request.trim()).map_err(|error| error.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&line).map_err(|error| format!("answer {line:?}: {error}"))
}

fn serve(args: &Args) {
    let profile = match args.get("profile-file") {
        Some(path) => Profile::parse(&read(path)),
        None => Profile::builtin(args.need("profile")),
    }
    .unwrap_or_else(|error| fail(error));
    let upstream: SocketAddr = args
        .need("upstream")
        .parse()
        .unwrap_or_else(|_| fail("--upstream is not addr:port"));
    let run_dir = PathBuf::from(args.need("run-dir"));
    std::fs::create_dir_all(&run_dir).unwrap_or_else(|error| fail(error));
    let mut config = RigConfig::new(profile, upstream, &read(args.need("upstream-ca")), &run_dir);
    config.broker_name = args.need("broker-name").to_ascii_lowercase();
    if let Some(seed) = args.get("seed") {
        config.seed = seed
            .parse()
            .unwrap_or_else(|_| fail("--seed is not a number"));
    }
    if let Some(listen) = args.get("listen") {
        config.listen = listen
            .parse()
            .unwrap_or_else(|_| fail("--listen is not addr:port"));
    }
    if let Some(admit) = args.get("admit") {
        config.admit = network_rig::admit::Admit::parse(admit).unwrap_or_else(|error| fail(error));
    }
    if let (Some(key), Some(cert)) = (args.get("edge-ca-key"), args.get("edge-ca-cert")) {
        config.edge_authority = Some(EdgeAuthority {
            common_name: args.get("edge-ca-name").map_or_else(
                || "network-rig edge authority".to_string(),
                |path| read(path).trim().to_string(),
            ),
            key_pem: read(key),
            cert_pem: read(cert),
        });
    }
    let host_addresses = args.has("edge-host-addresses");
    if host_addresses {
        config.edge_addresses = host_ipv4_addresses();
    }
    let listen = config.listen;
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let mut rig = Rig::start(config).unwrap_or_else(|error| fail(error));
    let port = rig.client_side().port;
    let socket = run_dir.join("control.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap_or_else(|error| fail(error));
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|error| fail(error));
    if !args.has("no-play") {
        log_notes(&run_dir, rig.play());
    }
    let record = json!({
        "pid": std::process::id(),
        "listen": listen.to_string(),
        "port": port,
        "url": rig.client_side().url,
        "profile": rig.profile().id,
        "seed": rig.schedule().seed,
    });
    std::fs::write(run_dir.join("serve.json"), format!("{record:#}\n"))
        .unwrap_or_else(|error| fail(error));
    println!("{record}");

    let mut checked = std::time::Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        // A guest network appears with its first VM: the edge starts again
        // with a certificate that names its host address too.
        if host_addresses && checked.elapsed() >= Duration::from_secs(2) {
            checked = std::time::Instant::now();
            let now = host_ipv4_addresses();
            if now != rig.edge_addresses() {
                eprintln!("network-rig: host addresses now {now:?}; restarting the edge");
                if let Err(error) = rig.set_edge_addresses(&now) {
                    eprintln!("network-rig: restarting the edge: {error}");
                }
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if handle(&mut rig, &run_dir, stream) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => fail(error),
        }
    }
    let _ = std::fs::remove_file(&socket);
    drop(rig);
}

/// This machine's IPv4 addresses, loopback excluded, sorted.
fn host_ipv4_addresses() -> Vec<String> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs
    // releases; every entry is read before the list is freed.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut entry = list;
        while !entry.is_null() {
            let address = (*entry).ifa_addr;
            if !address.is_null() && i32::from((*address).sa_family) == libc::AF_INET {
                let inet = &*(address as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(inet.sin_addr.s_addr));
                if !ip.is_loopback() {
                    found.push(ip.to_string());
                }
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found.sort();
    found.dedup();
    found
}

fn log_notes(run_dir: &Path, notes: std::sync::mpsc::Receiver<network_rig::Note>) {
    let path = run_dir.join("notes.jsonl");
    std::thread::spawn(move || {
        for note in notes {
            let line = json!({
                "at_ms": note.at.as_millis() as u64,
                "event": note.event,
                "phase": format!("{:?}", note.phase),
            });
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    });
}

/// Answer one control connection; true when asked to shut down.
fn handle(rig: &mut Rig, run_dir: &Path, stream: UnixStream) -> bool {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    let (answer, shutdown) = match serde_json::from_str::<Value>(&line) {
        Ok(request) => answer(rig, run_dir, &request),
        Err(error) => (json!({"error": format!("bad request: {error}")}), false),
    };
    let mut stream = &stream;
    let _ = writeln!(stream, "{answer}");
    shutdown
}

fn answer(rig: &mut Rig, run_dir: &Path, request: &Value) -> (Value, bool) {
    let control = rig.control().clone();
    let fault = |value: &Value| serde_json::from_value::<Fault>(value.clone());
    let done = |result: Result<(), network_rig::RigError>| match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"error": error.to_string()}),
    };
    match request {
        Value::String(name) if name == "connections" => (
            json!({"ok": true, "connections": control.connections().iter().map(|c| json!({
                "id": c.id,
                "client": c.client.to_string(),
                "broker_source": c.broker_source.map(|a| a.to_string()),
            })).collect::<Vec<_>>()}),
            false,
        ),
        Value::String(name) if name == "play" => {
            log_notes(run_dir, rig.play());
            (json!({"ok": true}), false)
        }
        Value::String(name) if name == "stop_playing" => {
            rig.stop_playing();
            (json!({"ok": true}), false)
        }
        Value::String(name) if name == "shutdown" => (json!({"ok": true}), true),
        Value::Object(map) if map.contains_key("cut") => {
            let side = map["cut"]
                .get("side")
                .cloned()
                .map(serde_json::from_value::<Side>)
                .unwrap_or(Ok(Side::Both));
            match side {
                Ok(side) => {
                    let cut = control.cut(Select::All, side);
                    (json!({"ok": true, "cut": cut}), false)
                }
                Err(error) => (json!({"error": error.to_string()}), false),
            }
        }
        Value::Object(map) if map.contains_key("apply") => match fault(&map["apply"]) {
            Ok(fault) => (done(control.apply(&fault)), false),
            Err(error) => (json!({"error": error.to_string()}), false),
        },
        Value::Object(map) if map.contains_key("revert") => match fault(&map["revert"]) {
            Ok(fault) => (done(control.revert(&fault)), false),
            Err(error) => (json!({"error": error.to_string()}), false),
        },
        other => (json!({"error": format!("unknown request {other}")}), false),
    }
}
