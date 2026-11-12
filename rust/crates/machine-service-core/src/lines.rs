//! Reading one image line's `pilot-images/images/<image>/line.conf`.
//!
//! The configuration files are zsh assignments, so they are sourced with zsh
//! and the required keys are echoed back. Only the image catalog's builder
//! calls this, in the daemon that owns `daemon.lock` (IC-4,
//! `docs/arch/image-catalog.md`); see [`crate::catalog`].

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::proc::{run_capture, ProcError};
use crate::ssh::shell_quote;

/// The keys read out of a `line.conf`.
pub const CONF_KEYS: [&str; 8] = [
    "LINE_KIND",
    "BASE_VM",
    "WORK_VM",
    "CLONE_PREFIX",
    "GUEST_USER",
    "GUEST_PASS",
    "CPU",
    "MEMORY_MB",
];

/// How long one `line.conf` may take to source.
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(15);

/// Source one `line.conf` strictly and return the requested keys.
///
/// Strict means a failing `source`, a nonzero exit, or a timeout fails the
/// file. The error names the cause without zsh's own diagnostic, which may
/// quote the file's contents, credentials included.
pub fn read_line_conf(conf_path: &Path) -> Result<Map<String, Value>, String> {
    let quoted = shell_quote(&conf_path.to_string_lossy());
    let prints: Vec<String> = CONF_KEYS
        .iter()
        .map(|key| format!("print -- \"{key}=${{{key}}}\""))
        .collect();
    let script = format!("source {quoted} || exit $?; {}", prints.join(" ; "));
    let mut command = service_layout::tools::command(service_layout::tools::Tool::Zsh)?;
    command.args(["-c", &script]);
    let output = run_capture(&mut command, None, SOURCE_TIMEOUT).map_err(|error| match error {
        ProcError::Timeout => format!(
            "line configuration parse failed: timed out after {} s",
            SOURCE_TIMEOUT.as_secs()
        ),
        other => format!("line configuration parse failed: {other}"),
    })?;
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(code) => format!("line configuration parse failed (zsh exit status {code})"),
            None => "line configuration parse failed (zsh killed by a signal)".to_string(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut values = Map::new();
    for line in crate::python::splitlines(&text) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if CONF_KEYS.contains(&key) {
            values.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    if !values.contains_key("BASE_VM") || !values.contains_key("CLONE_PREFIX") {
        return Err("line configuration parse failed (BASE_VM or CLONE_PREFIX missing)".into());
    }
    Ok(values)
}

/// The per-image resource defaults `(cpu, memory_mb, disk_gb)`.
///
/// An absent or empty CPU or memory value takes the service default; an
/// absent, empty or zero disk value means inherit (`None`). A value that is
/// not an integer is an error naming the key.
pub fn resource_defaults(conf: &Map<String, Value>) -> Result<(i64, i64, Option<i64>), String> {
    let number = |key: &str| -> Result<Option<i64>, String> {
        match conf.get(key).and_then(Value::as_str) {
            None | Some("") => Ok(None),
            Some(text) => text
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("invalid resource defaults: {key} is not an integer")),
        }
    };
    let cpu = number("CPU")?.unwrap_or(6);
    let memory_mb = number("MEMORY_MB")?.unwrap_or(16384);
    // An absent or zero disk value means inherit, represented as null.
    let disk_gb = number("DISK_GB")?.filter(|value| *value != 0);
    Ok((cpu, memory_mb, disk_gb))
}

/// The active-state tuple used for lease slot accounting.
pub const ACTIVE_STATES: [&str; 4] = ["pending", "provisioning", "running", "releasing"];
