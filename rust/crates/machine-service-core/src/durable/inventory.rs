//! The inventory before and after a durable lease (`docs/arch/hygiene.md`,
//! HY-4).
//!
//! An inventory is a set of independent parts. Each part runs on its own
//! thread with its own timeout; a part that fails or times out is recorded as
//! missing with its reason and the others are kept. An inventory never
//! refuses or blocks a lease (HY-1).
//!
//! The parts are the user's processes, the running regular applications,
//! the installed applications, the launchd jobs, the Homebrew formulae and
//! the casks (two parts), the global npm packages, the crontab, the
//! top-level entries of the home folder, `~/Library/Application Support`
//! and `~/Library/Preferences`, and the free disk space.
//!
//! The crontab is recorded without its commands' arguments (DC-74): an
//! entry is its schedule, its program and the SHA-256 of the whole line, so
//! a change shows and a token written into a crontab line never reaches a
//! hygiene record that any LAN client can read.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::process;

/// One inventory part: its name and the work that produces it.
pub type Part = (String, Box<dyn FnOnce() -> Result<Value, String> + Send>);

/// What the standard parts need to know.
#[derive(Debug, Clone)]
pub struct Context {
    /// The service's user ID.
    pub uid: u32,
    /// The account's home folder.
    pub home: PathBuf,
    /// The CI runner's folder (HY-5).
    pub ci_root: PathBuf,
    /// The workspace root, which changes with every lease and is left out.
    pub workspace_root: PathBuf,
    /// The session's environment, whose `PATH` finds `brew`.
    pub env: BTreeMap<String, String>,
    /// Each part's timeout.
    pub timeout: Duration,
}

/// Take an inventory from `parts`, each bounded by `timeout`.
pub fn take(parts: Vec<Part>, timeout: Duration) -> Value {
    let mut receivers = Vec::new();
    for (name, work) in parts {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receivers.push((name, receiver));
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut found = Map::new();
    let mut missing = Map::new();
    for (name, receiver) in receivers {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(Ok(value)) => {
                found.insert(name, value);
            }
            Ok(Err(reason)) => {
                missing.insert(name, Value::String(reason));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                missing.insert(
                    name,
                    Value::String(format!("timed out after {} s", timeout.as_secs_f64())),
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                missing.insert(name, Value::String("the part failed".to_string()));
            }
        }
    }
    json!({
        "taken_at": crate::config::unix_now(),
        "parts": found,
        "missing": missing,
    })
}

/// The standard parts of this first cut (HY-4).
pub fn standard_parts(context: &Context, software: Option<Value>) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let ctx = context.clone();
    parts.push((
        "processes".to_string(),
        Box::new(move || Ok(processes(&ctx))),
    ));
    let ctx = context.clone();
    parts.push((
        "applications".to_string(),
        Box::new(move || running_applications(ctx.timeout)),
    ));
    let ctx = context.clone();
    parts.push((
        "installed_applications".to_string(),
        Box::new(move || Ok(installed_applications(&ctx))),
    ));
    let ctx = context.clone();
    parts.push((
        "launchd_jobs".to_string(),
        Box::new(move || Ok(launchd_jobs(&ctx.home))),
    ));
    // Formulae and casks are separate parts: one failing (an untrusted tap
    // refuses `brew list --cask`) must not hide the other.
    let ctx = context.clone();
    parts.push((
        "homebrew_formulae".to_string(),
        Box::new(move || homebrew(&ctx, "--formula")),
    ));
    let ctx = context.clone();
    parts.push((
        "homebrew_casks".to_string(),
        Box::new(move || homebrew(&ctx, "--cask")),
    ));
    let ctx = context.clone();
    parts.push((
        "npm_globals".to_string(),
        Box::new(move || npm_globals(&ctx)),
    ));
    let ctx = context.clone();
    parts.push(("crontab".to_string(), Box::new(move || crontab(&ctx))));
    let ctx = context.clone();
    parts.push(("folders".to_string(), Box::new(move || Ok(folders(&ctx)))));
    parts.push((
        "disk".to_string(),
        Box::new(|| {
            super::capabilities::disk("/")
                .map(|(total, free)| json!({"total_bytes": total, "free_bytes": free}))
        }),
    ));
    if let Some(software) = software {
        parts.push(("software".to_string(), Box::new(move || Ok(software))));
    }
    parts
}

fn processes(context: &Context) -> Value {
    let table = process::process_table(Some(context.uid));
    let ci = process::ci_pids(&table, &context.ci_root, &process::process_arguments);
    Value::Array(
        table
            .iter()
            .filter(|info| !info.zombie)
            .map(|info| {
                let mut entry = json!({
                    "pid": info.pid,
                    "executable": info.executable(),
                    "started_at": info.started_at(),
                });
                if ci.contains(&info.pid) {
                    entry["origin"] = json!("ci");
                }
                entry
            })
            .collect(),
    )
}

/// The running regular applications, those with a Dock presence, from
/// `lsappinfo`.
pub fn running_applications(timeout: Duration) -> Result<Value, String> {
    // A test build's running applications are its fixture's (DC-253).
    if super::seam::active() {
        return Ok(super::seam::applications());
    }
    let mut command = service_layout::tools::command(service_layout::tools::Tool::Lsappinfo)?;
    command.arg("list");
    let output = crate::proc::run_capture(&mut command, None, timeout)
        .map_err(|error| format!("lsappinfo: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsappinfo exited with {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(Value::Array(parse_lsappinfo(&String::from_utf8_lossy(
        &output.stdout,
    ))))
}

/// Parse `lsappinfo list`, keeping the `Foreground` applications.
pub fn parse_lsappinfo(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut current: Option<Map<String, Value>> = None;
    let flush = |entry: Option<Map<String, Value>>, out: &mut Vec<Value>| {
        if let Some(entry) = entry {
            if entry.get("type").and_then(Value::as_str) == Some("Foreground") {
                let mut entry = entry;
                entry.remove("type");
                out.push(Value::Object(entry));
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some((number, rest)) = trimmed.split_once(") \"") {
            if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() {
                flush(current.take(), &mut out);
                let name = rest.split('"').next().unwrap_or_default();
                let mut entry = Map::new();
                entry.insert("name".to_string(), json!(name));
                current = Some(entry);
                continue;
            }
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(value) = quoted_after(trimmed, "bundleID=") {
            entry.insert("bundle_id".to_string(), json!(value));
        } else if let Some(value) = quoted_after(trimmed, "bundle path=") {
            entry.insert("path".to_string(), json!(value));
        } else if let Some(rest) = trimmed.strip_prefix("pid = ") {
            let pid = rest
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse::<i64>().ok());
            entry.insert("pid".to_string(), json!(pid));
            if let Some(kind) = rest
                .split("type=\"")
                .nth(1)
                .and_then(|tail| tail.split('"').next())
            {
                entry.insert("type".to_string(), json!(kind));
            }
        }
    }
    flush(current.take(), &mut out);
    out
}

fn quoted_after(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.strip_prefix('"')?;
    Some(rest.split('"').next().unwrap_or_default().to_string())
}

/// The bundle version of an application, from its `Info.plist`.
pub fn bundle_version(app: &Path, timeout: Duration) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    for key in ["CFBundleShortVersionString", "CFBundleVersion"] {
        let mut command =
            service_layout::tools::command(service_layout::tools::Tool::Plutil).ok()?;
        command
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist);
        if let Ok(output) = crate::proc::run_capture(&mut command, None, timeout) {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if output.status.success() && !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn installed_applications(context: &Context) -> Value {
    let mut out = Map::new();
    for folder in [
        PathBuf::from("/Applications"),
        context.home.join("Applications"),
    ] {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut apps: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
            .collect();
        apps.sort();
        for app in apps {
            let version = bundle_version(&app, context.timeout);
            out.insert(app.to_string_lossy().to_string(), json!(version));
        }
    }
    Value::Object(out)
}

/// Top-level entries of `folder` with their modification times.
fn entries_with_mtime(folder: &Path, skip: &[&Path]) -> Map<String, Value> {
    let mut out = Map::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if skip.iter().any(|skipped| *skipped == path) {
            continue;
        }
        let mtime = std::fs::symlink_metadata(&path)
            .map(|metadata| metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9)
            .ok();
        out.insert(path.to_string_lossy().to_string(), json!(mtime));
    }
    out
}

fn launchd_jobs(home: &Path) -> Value {
    let mut out = Map::new();
    for folder in [
        home.join("Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
    ] {
        out.extend(entries_with_mtime(&folder, &[]));
    }
    Value::Object(out)
}

fn folders(context: &Context) -> Value {
    // The CI runner's folder is never touched (HY-5), and the workspace
    // root changes with every lease by design.
    let workspace_top = context
        .workspace_root
        .ancestors()
        .find(|path| path.parent() == Some(context.home.as_path()))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| context.workspace_root.clone());
    let skip = [context.ci_root.as_path(), workspace_top.as_path()];
    let mut out = Map::new();
    for folder in [
        context.home.clone(),
        context.home.join("Library/Application Support"),
        context.home.join("Library/Preferences"),
    ] {
        out.extend(entries_with_mtime(&folder, &skip));
    }
    Value::Object(out)
}

/// Find an executable on a `PATH`; a test build runs what it finds only if
/// it is a test's own, never one installed in the system's folders
/// (DC-254).
pub fn which(name: &str, path: Option<&String>) -> Option<PathBuf> {
    found(name, path).filter(|found| {
        !service_layout::test_build::is_isolated()
            || service_layout::tools::program(&found.to_string_lossy()).is_ok()
    })
}

fn found(name: &str, path: Option<&String>) -> Option<PathBuf> {
    path?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
        })
}

fn homebrew(context: &Context, kind: &str) -> Result<Value, String> {
    let brew = which("brew", context.env.get("PATH"))
        .ok_or_else(|| "brew not found on the session PATH".to_string())?;
    let mut command = Command::new(&brew);
    command
        .args(["list", kind, "--versions"])
        .env_clear()
        .envs(&context.env)
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_ANALYTICS", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .current_dir(&context.home);
    let output = crate::proc::run_capture(&mut command, None, context.timeout)
        .map_err(|error| format!("brew list {kind}: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect::<String>();
        return Err(format!(
            "brew list {kind} exited with {}: {reason}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let mut out = Map::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut words = line.split_whitespace();
        if let Some(name) = words.next() {
            let versions: Vec<&str> = words.collect();
            out.insert(name.to_string(), json!(versions.join(" ")));
        }
    }
    Ok(Value::Object(out))
}

/// The global npm packages, `{name: version}`, from the session's `npm`.
fn npm_globals(context: &Context) -> Result<Value, String> {
    let npm = which("npm", context.env.get("PATH"))
        .ok_or_else(|| "npm not found on the session PATH".to_string())?;
    let mut command = Command::new(&npm);
    command
        .args(["ls", "--global", "--depth=0", "--json"])
        .env_clear()
        .envs(&context.env)
        .env("NO_UPDATE_NOTIFIER", "1")
        .env("npm_config_update_notifier", "false")
        .current_dir(&context.home);
    let output = crate::proc::run_capture(&mut command, None, context.timeout)
        .map_err(|error| format!("npm ls --global: {error}"))?;
    // `npm ls` exits non-zero for a problem in the tree and still answers.
    parse_npm_globals(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        format!(
            "npm ls --global exited with {} and no JSON",
            output.status.code().unwrap_or(-1)
        )
    })
}

/// Parse `npm ls --global --depth=0 --json` into `{name: version}`.
pub fn parse_npm_globals(text: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let mut out = Map::new();
    if let Some(dependencies) = parsed.get("dependencies").and_then(Value::as_object) {
        for (name, entry) in dependencies {
            out.insert(
                name.clone(),
                entry.get("version").cloned().unwrap_or(Value::Null),
            );
        }
    }
    Some(Value::Object(out))
}

/// The user's crontab, one entry per line (DC-74).
fn crontab(context: &Context) -> Result<Value, String> {
    let mut command = service_layout::tools::command(service_layout::tools::Tool::Crontab)?;
    command
        .arg("-l")
        .env_clear()
        .envs(&context.env)
        .current_dir(&context.home);
    let output = crate::proc::run_capture(&mut command, None, context.timeout)
        .map_err(|error| format!("crontab -l: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).to_string();
        if reason.contains("no crontab for") {
            return Ok(json!({}));
        }
        return Err(format!(
            "crontab -l exited with {}: {}",
            output.status.code().unwrap_or(-1),
            reason.lines().next().unwrap_or_default()
        ));
    }
    Ok(parse_crontab(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse a crontab into `{sha256: {schedule, program}}`: comments, blank
/// lines and variable settings are left out, and no argument is kept.
pub fn parse_crontab(text: &str) -> Value {
    use sha2::{Digest, Sha256};
    let mut out = Map::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let first = words.first().copied().unwrap_or_default();
        if !first.starts_with('@') && first.contains('=') {
            continue;
        }
        let fields = if first.starts_with('@') { 1 } else { 5 };
        if words.len() <= fields {
            continue;
        }
        let digest: String = Sha256::digest(line.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        out.insert(
            digest,
            json!({
                "schedule": words[..fields].join(" "),
                "program": words[fields],
            }),
        );
    }
    Value::Object(out)
}

/// Compare two inventories into `changes.json` (HY-4). CI-runner processes
/// keep their `origin` so a client does not attribute them to the lease.
pub fn diff(before: &Value, after: &Value) -> Value {
    let part = |inventory: &Value, name: &str| inventory["parts"].get(name).cloned();
    let mut parts = Map::new();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for inventory in [before, after] {
        if let Some(map) = inventory.get("parts").and_then(Value::as_object) {
            names.extend(map.keys().cloned());
        }
    }
    for name in names {
        let (Some(old), Some(new)) = (part(before, &name), part(after, &name)) else {
            continue;
        };
        let change = match name.as_str() {
            "processes" => list_diff(&old, &new, |entry| {
                format!(
                    "{}:{}",
                    entry["pid"],
                    entry["started_at"].as_f64().unwrap_or_default()
                )
            }),
            "applications" => list_diff(&old, &new, |entry| {
                entry
                    .get("path")
                    .or_else(|| entry.get("name"))
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            }),
            "disk" => json!({
                "free_bytes_before": old["free_bytes"],
                "free_bytes_after": new["free_bytes"],
            }),
            _ => map_diff(&old, &new),
        };
        parts.insert(name, change);
    }
    let missing = |inventory: &Value| inventory.get("missing").cloned().unwrap_or(json!({}));
    json!({
        "parts": parts,
        "missing": {"before": missing(before), "after": missing(after)},
    })
}

fn list_diff(old: &Value, new: &Value, key: impl Fn(&Value) -> String) -> Value {
    let empty = Vec::new();
    let old = old.as_array().unwrap_or(&empty);
    let new = new.as_array().unwrap_or(&empty);
    let old_keys: HashSet<String> = old.iter().map(&key).collect();
    let new_keys: HashSet<String> = new.iter().map(&key).collect();
    json!({
        "added": new.iter().filter(|entry| !old_keys.contains(&key(entry))).cloned().collect::<Vec<_>>(),
        "removed": old.iter().filter(|entry| !new_keys.contains(&key(entry))).cloned().collect::<Vec<_>>(),
    })
}

fn map_diff(old: &Value, new: &Value) -> Value {
    let empty = Map::new();
    let old = old.as_object().unwrap_or(&empty);
    let new = new.as_object().unwrap_or(&empty);
    let mut added = Map::new();
    let mut removed = Map::new();
    let mut changed = Map::new();
    for (key, value) in new {
        match old.get(key) {
            None => {
                added.insert(key.clone(), value.clone());
            }
            Some(previous) if previous != value => {
                changed.insert(key.clone(), json!({"before": previous, "after": value}));
            }
            Some(_) => {}
        }
    }
    for (key, value) in old {
        if !new.contains_key(key) {
            removed.insert(key.clone(), value.clone());
        }
    }
    json!({"added": added, "removed": removed, "changed": changed})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HY-4 (TL-1): global npm packages by name and version; a crontab by
    /// schedule and program, with no argument and no variable kept.
    #[test]
    fn hy4_npm_and_crontab_parts() {
        let npm = parse_npm_globals(
            r#"{"name": "lib", "dependencies": {"@openai/codex": {"version": "0.154.0"}, "npm": {"version": "11.6.2"}}}"#,
        )
        .unwrap();
        assert_eq!(npm["@openai/codex"], "0.154.0");
        assert_eq!(npm["npm"], "11.6.2");
        assert_eq!(parse_npm_globals("{}").unwrap(), json!({}));
        assert!(parse_npm_globals("not json").is_none());

        let crontab = parse_crontab(
            "# comment\nTOKEN=secret-value\n*/5 * * * * /usr/local/bin/backup --token secret-arg\n@reboot /bin/echo hi\n\n",
        );
        let entries: Vec<&Value> = crontab.as_object().unwrap().values().collect();
        assert_eq!(entries.len(), 2);
        let text = crontab.to_string();
        assert!(!text.contains("secret"), "{text}");
        assert!(entries
            .iter()
            .any(|entry| entry["schedule"] == "*/5 * * * *"
                && entry["program"] == "/usr/local/bin/backup"));
        assert!(entries
            .iter()
            .any(|entry| entry["schedule"] == "@reboot" && entry["program"] == "/bin/echo"));
    }

    /// AC-31 (TL-1): a part that times out is recorded as missing with its
    /// reason, and the other parts are kept.
    #[test]
    fn ac31_timed_out_part_is_missing_and_others_kept() {
        let parts: Vec<Part> = vec![
            ("fast".to_string(), Box::new(|| Ok(json!({"a": 1})))),
            (
                "slow".to_string(),
                Box::new(|| {
                    std::thread::sleep(Duration::from_secs(5));
                    Ok(json!({}))
                }),
            ),
            (
                "broken".to_string(),
                Box::new(|| Err("brew not found on the session PATH".to_string())),
            ),
        ];
        let begun = std::time::Instant::now();
        let inventory = take(parts, Duration::from_millis(300));
        assert!(begun.elapsed() < Duration::from_secs(3));
        assert_eq!(inventory["parts"]["fast"], json!({"a": 1}));
        assert!(inventory["missing"]["slow"]
            .as_str()
            .unwrap()
            .starts_with("timed out after"));
        assert_eq!(
            inventory["missing"]["broken"],
            "brew not found on the session PATH"
        );
    }

    /// AC-31 (TL-1): changes list an added formula, an added and a changed
    /// Application Support entry, and keep a CI process's origin.
    #[test]
    fn ac31_diff_reports_additions_and_changes() {
        let before = json!({"parts": {
            "homebrew_formulae": {"jq": "1.7"},
            "folders": {"/h/Library/Application Support/A": 1.0},
            "processes": [{"pid": 1, "started_at": 1.0, "executable": "/bin/a"}],
        }, "missing": {}});
        let after = json!({"parts": {
            "homebrew_formulae": {"jq": "1.7", "hello": "2.12"},
            "folders": {
                "/h/Library/Application Support/A": 2.0,
                "/h/Library/Application Support/B": 3.0,
            },
            "processes": [
                {"pid": 1, "started_at": 1.0, "executable": "/bin/a"},
                {"pid": 9, "started_at": 5.0, "executable": "/ci/Runner.Worker", "origin": "ci"},
            ],
        }, "missing": {"applications": "timed out after 60 s"}});
        let changes = diff(&before, &after);
        assert_eq!(
            changes["parts"]["homebrew_formulae"]["added"],
            json!({"hello": "2.12"})
        );
        assert_eq!(
            changes["parts"]["folders"]["added"],
            json!({"/h/Library/Application Support/B": 3.0})
        );
        assert_eq!(
            changes["parts"]["folders"]["changed"]["/h/Library/Application Support/A"],
            json!({"before": 1.0, "after": 2.0})
        );
        assert_eq!(
            changes["parts"]["processes"]["added"][0]["origin"],
            json!("ci")
        );
        assert_eq!(
            changes["missing"]["after"]["applications"],
            "timed out after 60 s"
        );
    }

    #[test]
    fn lsappinfo_keeps_foreground_applications() {
        let text = r#" 1) "loginwindow" ASN:0x0-0x1001:
    bundleID="com.apple.loginwindow"
    bundle path="/System/Library/CoreServices/loginwindow.app"
    pid = 450 token=[sess=1] type="UIElement" flavor=3
 2) "TextEdit" ASN:0x0-0x2002:
    bundleID="com.apple.TextEdit"
    bundle path="/System/Applications/TextEdit.app"
    executable path="/System/Applications/TextEdit.app/Contents/MacOS/TextEdit"
    pid = 777 type="Foreground" flavor=3 Version="1"
"#;
        assert_eq!(
            parse_lsappinfo(text),
            vec![json!({
                "name": "TextEdit",
                "bundle_id": "com.apple.TextEdit",
                "path": "/System/Applications/TextEdit.app",
                "pid": 777,
            })]
        );
    }
}
