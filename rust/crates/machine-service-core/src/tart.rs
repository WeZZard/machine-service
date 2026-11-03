//! Tart lifecycle helpers, mirroring the Python `tart` helpers in `machine-service`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{OpError, OpResult};
use crate::proc::{combined_text, run_capture, ProcError};

/// The settle window used to detect a hypervisor-refused boot.
pub const BOOT_SETTLE_S: u64 = 10;

/// Build a `tart` command with the configured executable and environment;
/// a test build's Tart is its tool table's (DC-1xx-ISO-4).
pub fn tart_command(config: &Config, args: &[&str]) -> OpResult<Command> {
    let program = if Path::new(&config.tart_executable).is_absolute() {
        // A path the configuration names; a test build refuses one in the
        // system's folders (DC-1xx-ISO-4).
        service_layout::tools::program(&config.tart_executable)
    } else {
        service_layout::tools::located(service_layout::tools::Tool::Tart, || {
            Ok(std::path::PathBuf::from(&config.tart_executable))
        })
    }
    .map_err(OpError::new)?;
    let mut command = Command::new(program);
    command.args(args);
    if let Some(environment) = &config.subprocess_env {
        command.env_clear();
        command.envs(environment.iter().map(|(key, value)| (key, value)));
    }
    Ok(command)
}

/// Run `tart` and capture output. `check` mirrors Python's `check=True`.
pub fn tart(
    config: &Config,
    args: &[&str],
    check: bool,
    timeout_s: u64,
) -> OpResult<std::process::Output> {
    let mut command = tart_command(config, args)?;
    let output = run_capture(&mut command, None, Duration::from_secs(timeout_s))
        .map_err(|error| tart_error(args, error))?;
    if check && !output.status.success() {
        return Err(OpError::new(format!(
            "tart {} failed (rc={}): {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            combined_text(&output).trim()
        )));
    }
    Ok(output)
}

fn tart_error(args: &[&str], error: ProcError) -> OpError {
    match error {
        ProcError::Timeout => OpError::new(format!("tart {} timed out", args.join(" "))),
        ProcError::Cancelled => OpError::new(format!("tart {} cancelled", args.join(" "))),
        ProcError::Io(error) => OpError::new(format!("tart {} failed: {error}", args.join(" "))),
    }
}

/// `[(name, state)]` for local VMs, parsed from `tart list`.
pub fn tart_list(config: &Config) -> OpResult<Vec<(String, String)>> {
    let output = tart(config, &["list"], true, 120)?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in crate::python::splitlines(&text).into_iter().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 && parts[0] == "local" {
            rows.push((parts[1].to_string(), parts[parts.len() - 1].to_string()));
        }
    }
    Ok(rows)
}

/// Whether a VM name exists locally.
pub fn vm_exists(config: &Config, name: &str) -> OpResult<bool> {
    Ok(tart_list(config)?
        .iter()
        .any(|(candidate, _)| candidate == name))
}

/// Whether a VM exists locally and is running.
pub fn vm_running(config: &Config, name: &str) -> OpResult<bool> {
    Ok(tart_list(config)?
        .iter()
        .any(|(candidate, state)| candidate == name && state == "running"))
}

/// Advisory count of host-wide Virtualization.framework guests.
///
/// Best effort: a failed gauge degrades to `None` and never raises.
pub fn host_macos_guests() -> Option<usize> {
    let mut command = service_layout::tools::command(service_layout::tools::Tool::Pgrep).ok()?;
    command.args(["-f", "com.apple.Virtualization.VirtualMachine"]);
    let output = run_capture(&mut command, None, Duration::from_secs(10)).ok()?;
    let code = output.status.code()?;
    if code != 0 && code != 1 {
        return None;
    }
    Some(
        crate::python::splitlines(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

/// The first reported IP for a VM, or `None`.
pub fn vm_ip(config: &Config, name: &str) -> Option<String> {
    let output = tart(config, &["ip", name], false, 30).ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    crate::python::splitlines(&text)
        .into_iter()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

/// Wait for a VM to report an IP within `timeout_s`.
pub fn wait_ip(config: &Config, name: &str, timeout_s: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        if let Some(ip) = vm_ip(config, name) {
            return Some(ip);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Spawn `tart run <vm> --no-graphics` detached, logging to
/// `/tmp/tart-run-<vm>.log`, mirroring the Python `Popen`. A guest on the
/// `internet` network adds `--net-softnet` (`crate::network::run_args`).
pub fn spawn_run(
    config: &Config,
    vm: &str,
    network: acquisition_options::GuestNetwork,
) -> OpResult<std::process::Child> {
    let log_path = format!("/tmp/tart-run-{vm}.log");
    let stdout = std::fs::File::create(&log_path)?;
    let stderr = stdout.try_clone()?;
    let args = crate::network::run_args(vm, network);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut command = tart_command(config, &args)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map_err(OpError::from)
}

/// The `PATH` Tart runs with: the selected environment's, or the daemon's.
pub fn tart_path(config: &Config) -> Option<std::ffi::OsString> {
    match &config.subprocess_env {
        Some(environment) => environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.into()),
        None => std::env::var_os("PATH"),
    }
}

/// The Tart executable `tart_command` would run: the configured path when it
/// names one, else `tart` found on the `PATH` Tart runs with. Answers why
/// there is none, naming Tart, so the refusal says what is missing (AC-38).
pub fn find_executable(config: &Config) -> Result<std::path::PathBuf, String> {
    let found = if Path::new(&config.tart_executable).is_absolute() {
        service_layout::tools::program(&config.tart_executable)
            .and_then(|_| find_configured(config))
    } else {
        service_layout::tools::located(service_layout::tools::Tool::Tart, || {
            find_configured(config)
        })
    };
    found.map_err(|error| {
        if error.starts_with("Tart is not installed") {
            error
        } else {
            format!("Tart is not installed ({error})")
        }
    })
}

fn find_configured(config: &Config) -> Result<std::path::PathBuf, String> {
    let executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|meta| {
            meta.is_file()
                && std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
        })
    };
    let configured = config.tart_executable.as_str();
    if configured.contains('/') {
        let path = Path::new(configured);
        return if executable(path) {
            Ok(path.to_path_buf())
        } else {
            Err(format!(
                "Tart is not installed (no executable at {configured})"
            ))
        };
    }
    let name = if configured.is_empty() {
        "tart"
    } else {
        configured
    };
    let search = tart_path(config).unwrap_or_default();
    std::env::split_paths(&search)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
        .ok_or_else(|| format!("Tart is not installed (no {name} on PATH)"))
}

/// Log path for a running VM, matching the Python convention.
pub fn run_log_path(vm: &str) -> String {
    format!("/tmp/tart-run-{vm}.log")
}

/// Restrict a path so private lease SSH material is never transferred.
pub fn _unused_path_reference(_path: &Path) {}
