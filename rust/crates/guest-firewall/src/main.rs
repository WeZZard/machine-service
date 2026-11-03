//! `vm-service-guest-firewall`: install, remove, or show the pf anchor that
//! keeps `internet` guests off this host (DC-14).
//!
//! `install` and `remove` need root once; `print` and `status` do not.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use guest_firewall::{
    anchor_rules, daemon_plist, parse_anchor_broker_port, parse_anchor_table, parse_table, Line,
    ANCHOR, ANCHOR_FILE, DAEMON_PLIST, DEFAULT_BROKER_PORT, DEFAULT_TABLE_FILE, LABEL,
};

#[derive(Parser, Debug)]
#[command(
    name = "vm-service-guest-firewall",
    about = "Keep machine-service's internet guests off this host with a pf anchor (DC-14)."
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand, Debug)]
enum Action {
    /// Write the anchor, its table file and its LaunchDaemon, enable pf, and load the anchor now (root).
    Install {
        /// The table file every machine-service instance on this host writes.
        #[arg(long, default_value = DEFAULT_TABLE_FILE)]
        table: PathBuf,
        /// The numeric user that runs machine-service and owns the table file;
        /// defaults to the user that ran sudo.
        #[arg(long)]
        owner: Option<u32>,
        #[command(flatten)]
        broker: Broker,
    },
    /// Unload the anchor and remove its file and LaunchDaemon (root). pf stays enabled.
    Remove,
    /// Print the anchor rules and the LaunchDaemon without installing them.
    Print {
        /// The table file every machine-service instance on this host writes.
        #[arg(long, default_value = DEFAULT_TABLE_FILE)]
        table: PathBuf,
        #[command(flatten)]
        broker: Broker,
    },
    /// Report what is installed, without root.
    Status,
}

/// The one host port the guests may reach (DC-107).
#[derive(clap::Args, Debug)]
struct Broker {
    /// The broker's port, machine-service's session listener, that internet
    /// guests may reach on this host (DC-107).
    #[arg(long, default_value_t = DEFAULT_BROKER_PORT)]
    broker_port: u16,
    /// Let internet guests reach no port of this host, not even the broker's.
    #[arg(long, conflicts_with = "broker_port")]
    no_broker: bool,
}

impl Broker {
    fn port(&self) -> Option<u16> {
        (!self.no_broker).then_some(self.broker_port)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Action::Install {
            table,
            owner,
            broker,
        } => install(&table, owner, broker.port()),
        Action::Remove => remove(),
        Action::Print { table, broker } => {
            print!(
                "# {ANCHOR_FILE}\n{}\n# {DAEMON_PLIST}\n{}",
                anchor_rules(&table, broker.port()),
                daemon_plist(&table)
            );
            Ok(())
        }
        Action::Status => status(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("vm-service-guest-firewall: {message}");
            ExitCode::FAILURE
        }
    }
}

fn require_root(action: &str) -> Result<(), String> {
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let exe = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "vm-service-guest-firewall".to_string());
        return Err(format!("{action} needs root: run `sudo {exe} {action}`"));
    }
    Ok(())
}

fn install(table: &Path, owner: Option<u32>, broker_port: Option<u16>) -> Result<(), String> {
    require_root("install")?;
    if !table.is_absolute() {
        return Err(format!("--table must be absolute: {}", table.display()));
    }
    let (uid, gid) = owner_ids(owner)?;
    prepare_table(table, uid, gid)?;

    let rules = anchor_rules(table, broker_port);
    let candidate = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    std::fs::write(candidate.path(), &rules).map_err(|error| error.to_string())?;
    run(
        "/sbin/pfctl",
        &["-nf", &candidate.path().display().to_string()],
    )?;

    write_root_file(Path::new(ANCHOR_FILE), &rules)?;
    write_root_file(Path::new(DAEMON_PLIST), &daemon_plist(table))?;
    run("/usr/bin/plutil", &["-lint", DAEMON_PLIST])?;

    // Replace a previous daemon; its absence is not an error.
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .output();
    run("/bin/launchctl", &["bootstrap", "system", DAEMON_PLIST])?;

    // RunAtLoad enables pf and loads the anchor; wait for both.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let loaded = capture("/sbin/pfctl", &["-a", ANCHOR, "-s", "rules"]).unwrap_or_default();
        let enabled = capture("/sbin/pfctl", &["-s", "info"])
            .map(|info| info.lines().any(|line| line.starts_with("Status: Enabled")))
            .unwrap_or(false);
        if loaded.contains("block drop in quick") && enabled {
            println!(
                "installed: pf is enabled and anchor {ANCHOR} drops new connections to this host from the guests in {}, and limits the egress guests of {} (DC-1xx-EPH-9)",
                table.display(),
                guest_firewall::EgressFiles::beside(table).guests.display()
            );
            println!("{}", loaded.trim_end());
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the LaunchDaemon {LABEL} did not load the anchor within 10 s; see {}",
                guest_firewall::DAEMON_LOG
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The account that owns the table file: `--owner`, or the user that ran
/// sudo, with that user's primary group.
fn owner_ids(owner: Option<u32>) -> Result<(u32, u32), String> {
    let uid = match owner {
        Some(uid) => uid,
        None => std::env::var("SUDO_UID")
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or(
                "run it with sudo from the account that runs machine-service, or pass --owner UID",
            )?,
    };
    if uid == 0 {
        return Err(
            "the table file must belong to the account that runs machine-service, not root".into(),
        );
    }
    // SAFETY: `getpwuid` returns static storage, read at once.
    let gid = unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() {
            return Err(format!("no account with uid {uid}"));
        }
        (*entry).pw_gid
    };
    Ok((uid, gid))
}

/// Create the table file's directory and an empty table file, owned by the
/// account that runs machine-service, so every instance on the host can write it
/// and pf can read it.
fn prepare_table(table: &Path, uid: u32, gid: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let directory = table.parent().ok_or("the table file has no directory")?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    chown(directory, uid, gid)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| error.to_string())?;
    if !table.exists() {
        std::fs::write(table, guest_firewall::render_table(&[]))
            .map_err(|error| format!("{}: {error}", table.display()))?;
    }
    chown(table, uid, gid)?;
    std::fs::set_permissions(table, std::fs::Permissions::from_mode(0o644))
        .map_err(|error| error.to_string())?;
    // The egress section's files (DC-1xx-EPH-9): pf refuses an anchor whose
    // table file is missing, so each exists, empty until it is written.
    let egress = guest_firewall::EgressFiles::beside(table);
    for file in egress.all() {
        if !file.exists() {
            guest_firewall::write_addresses(file, &[], &[])
                .map_err(|error| format!("{}: {error}", file.display()))?;
        }
        chown(file, uid, gid)?;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn chown(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|error| error.to_string())?;
    // SAFETY: the path is a valid C string for the duration of the call.
    if unsafe { libc::chown(c_path.as_ptr(), uid, gid) } != 0 {
        return Err(format!(
            "chown {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn remove() -> Result<(), String> {
    require_root("remove")?;
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .output();
    remove_if_present(Path::new(DAEMON_PLIST))?;
    run("/sbin/pfctl", &["-a", ANCHOR, "-F", "all"])?;
    remove_if_present(Path::new(ANCHOR_FILE))?;
    println!("removed: anchor {ANCHOR} is flushed; pf stays enabled with the system's rules");
    println!(
        "the table file's directory is kept for running machine-service instances; delete it by hand"
    );
    Ok(())
}

fn status() -> Result<(), String> {
    let table = match std::fs::read_to_string(ANCHOR_FILE) {
        Ok(text) => match parse_anchor_table(&text) {
            Some(table) => {
                println!(
                    "anchor file: {ANCHOR_FILE} (table file {})",
                    table.display()
                );
                match guest_firewall::parse_anchor_egress(&text) {
                    Some(files) => println!(
                        "egress section: guests {}, allow {}, lab {} (DC-1xx-EPH-9)",
                        files.guests.display(),
                        files.allow.display(),
                        files.lab.display()
                    ),
                    None => println!("egress section: none; egress leases are refused (reinstall for DC-1xx-EPH-9)"),
                }
                match parse_anchor_broker_port(&text) {
                    Some(port) => println!("broker port internet guests may reach: {port} (DC-107)"),
                    None => println!("broker port: none; internet guests reach no host port (reinstall for DC-107)"),
                }
                Some(table)
            }
            None => {
                println!("anchor file: {ANCHOR_FILE} (no table file; reinstall it)");
                None
            }
        },
        Err(_) => {
            println!("anchor file: missing ({ANCHOR_FILE})");
            None
        }
    };
    if Path::new(DAEMON_PLIST).exists() {
        println!("LaunchDaemon: {DAEMON_PLIST}");
    } else {
        println!("LaunchDaemon: missing ({DAEMON_PLIST})");
    }
    if let Some(table) = table {
        match std::fs::read_to_string(&table) {
            Ok(text) => {
                for line in parse_table(&text) {
                    if let Line::Guest(guest) = line {
                        println!(
                            "guest: {} ({} in {})",
                            guest.address, guest.vm, guest.instance
                        );
                    }
                }
            }
            Err(error) => println!("table file: unreadable ({error})"),
        }
    }
    println!("loaded rules and pf status need root: sudo pfctl -a {ANCHOR} -s rules; sudo pfctl -a {ANCHOR} -t {} -T show; sudo pfctl -s info", guest_firewall::TABLE);
    Ok(())
}

/// Write a root-owned, world-readable file by renaming a sibling temporary
/// file over it.
fn write_root_file(path: &Path, contents: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let directory = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let file = tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    std::fs::write(file.path(), contents).map_err(|error| error.to_string())?;
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644))
        .map_err(|error| error.to_string())?;
    // SAFETY: the path is a valid C string for the duration of the call.
    let c_path = std::ffi::CString::new(file.path().as_os_str().as_encoded_bytes())
        .map_err(|error| error.to_string())?;
    if unsafe { libc::chown(c_path.as_ptr(), 0, 0) } != 0 {
        return Err(format!(
            "chown {}: {}",
            file.path().display(),
            std::io::Error::last_os_error()
        ));
    }
    file.persist(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
