//! The suite's client is mcp-relay, driven over MCP as an agent drives it
//! (TL-7, DC-341, DC-342).
//!
//! [`McpRelay`] starts mcp-relay's MCP server (`node <server.mjs>`), speaks
//! MCP's JSON-RPC over the server's standard input and output, and calls its
//! tools: `relay_acquire`, `relay_stage`, `relay_script` and `relay_release`.
//! The server's standard error, where mcp-relay logs every loss and resume of
//! a lease's relay session (mcp-relay DC-85), is kept in a file of the run's
//! evidence, so a check can read the session's history.
//!
//! A `relay_script` result becomes an [`Answer`] with the script's exit and
//! its standard output and error, so a check written for the suite's own
//! client reads it with the same `rc()` and `text()`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use relay_agent::protocol::{CallResult, Digest, Op, Outcome};
use serde_json::{json, Value};

use crate::client::{exec_op, new_id, sha256_hex, Answer, RelayClient};

/// The outputs the suite's checks bring home with `relay_extract`, declared
/// when the lease is acquired, as mcp-relay requires: (name, path in the
/// guest's workspace).
pub const EXTRACTIONS: [(&str, &str); 1] = [("ac57-output", "acc/ac57.bin")];

/// The MCP protocol version the suite speaks.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How to start mcp-relay's server.
#[derive(Clone, Debug)]
pub struct McpSettings {
    /// `node`, or the path of the Node.js to run the server with.
    pub node: PathBuf,
    /// mcp-relay's `dist/server.mjs`.
    pub server: PathBuf,
    /// The server's project folder, where its evidence goes.
    pub project: PathBuf,
    /// The variables the server runs with, besides PATH and HOME: the
    /// machine-service origin (`MCP_RELAY_URL`), its state folder, and the
    /// rig's route to the broker (`MCP_RELAY_BROKER_RESOLVE`,
    /// `MCP_RELAY_BROKER_CA`).
    pub env: Vec<(String, String)>,
    /// Where the server's standard error goes.
    pub log: PathBuf,
}

/// One running mcp-relay server.
pub struct McpRelay {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next: u64,
    log: PathBuf,
}

/// What one tool call answered: its JSON parts and whether it is an error.
#[derive(Clone, Debug)]
pub struct ToolAnswer {
    /// Whether the tool reported an error (`isError`).
    pub is_error: bool,
    /// Each text part, parsed as JSON where it is JSON.
    pub parts: Vec<Value>,
}

impl ToolAnswer {
    /// The last part that is a JSON object holding `key`.
    pub fn with(&self, key: &str) -> Option<&Value> {
        self.parts.iter().rev().find(|part| part.get(key).is_some())
    }
}

impl McpRelay {
    /// Start the server and complete MCP's initialization.
    pub fn start(settings: &McpSettings) -> Result<McpRelay, String> {
        std::fs::create_dir_all(&settings.project).map_err(|e| format!("project folder: {e}"))?;
        let log = std::fs::File::create(&settings.log).map_err(|e| format!("server log: {e}"))?;
        let mut command = Command::new(&settings.node);
        command
            .arg(&settings.server)
            .current_dir(&settings.project)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log));
        for (key, value) in &settings.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("start mcp-relay: {e}"))?;
        let stdin = child.stdin.take().ok_or("mcp-relay has no stdin")?;
        let stdout = child.stdout.take().ok_or("mcp-relay has no stdout")?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut relay = McpRelay {
            child,
            stdin,
            lines,
            next: 1,
            log: settings.log.clone(),
        };
        relay.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "relay-acceptance", "version": env!("CARGO_PKG_VERSION")},
            }),
            Duration::from_secs(60),
        )?;
        relay.notify("notifications/initialized", json!({}))?;
        Ok(relay)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(message).map_err(|e| e.to_string())?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("write to mcp-relay: {e}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// One request and its response, within `deadline`; notifications and
    /// requests from the server in between are skipped.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let until = Instant::now() + deadline;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "mcp-relay did not answer {method} within {} s",
                        deadline.as_secs()
                    ))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "mcp-relay ended during {method}; its log: {}",
                        self.log.display()
                    ))
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id)
                || message.get("method").is_some()
            {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("mcp-relay refused {method}: {error}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Call one tool and parse its text parts.
    pub fn call(
        &mut self,
        tool: &str,
        arguments: Value,
        deadline: Duration,
    ) -> Result<ToolAnswer, String> {
        let result = self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            deadline,
        )?;
        let parts = result
            .get("content")
            .and_then(Value::as_array)
            .map(|content| {
                content
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .flat_map(json_values)
                    .collect()
            })
            .unwrap_or_default();
        Ok(ToolAnswer {
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            parts,
        })
    }

    /// `relay_acquire` of an image or a durable machine; answers the guest's name.
    pub fn acquire(
        &mut self,
        task: &str,
        what: Acquire<'_>,
        ttl_hours: f64,
    ) -> Result<String, String> {
        let declared: Vec<Value> = EXTRACTIONS
            .iter()
            .map(|(name, path)| json!({"name": name, "path": path}))
            .collect();
        let mut arguments = json!({"task": task, "extractions": declared, "ttlHours": ttl_hours});
        match what {
            Acquire::Image(image) => arguments["image"] = json!(image),
            Acquire::Machine(machine) => arguments["machine"] = json!(machine),
        }
        let answer = self.call("relay_acquire", arguments, Duration::from_secs(40 * 60))?;
        match answer
            .with("guest")
            .and_then(|part| part.get("guest"))
            .and_then(Value::as_str)
        {
            Some(guest) if !answer.is_error => Ok(guest.to_string()),
            _ => Err(format!("relay_acquire refused: {:?}", answer.parts)),
        }
    }

    /// `relay_stage` of the guest runtime.
    pub fn stage(&mut self, guest: &str) -> Result<ToolAnswer, String> {
        let answer = self.call(
            "relay_stage",
            json!({"guest": guest}),
            Duration::from_secs(10 * 60),
        )?;
        if answer.is_error {
            return Err(format!("relay_stage refused: {:?}", answer.parts));
        }
        Ok(answer)
    }

    /// `relay_script` of a shell script; its result as an [`Answer`].
    pub fn script(&mut self, guest: &str, code: &str, timeout: Duration) -> Result<Answer, String> {
        let started = Instant::now();
        let timeout_ms = u64::try_from(timeout.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let answer = self.call(
            "relay_script",
            json!({"guest": guest, "language": "shell", "inline": true, "code": code, "timeoutMs": timeout_ms}),
            timeout + Duration::from_secs(10 * 60),
        )?;
        script_answer(&answer, started.elapsed())
    }

    /// `relay_script` of a shell script that the suite cancels after `after`,
    /// as an agent cancels a tool call: MCP's `notifications/cancelled` for
    /// the call, which mcp-relay turns into a cancel for the execution
    /// (mcp-relay DC-50). A cancelled request need not be answered, so any
    /// answer within `drain` is read and dropped.
    pub fn script_cancelled(
        &mut self,
        guest: &str,
        code: &str,
        after: Duration,
        drain: Duration,
    ) -> Result<(), String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "relay_script", "arguments": {
                "guest": guest, "language": "shell", "inline": true, "code": code, "timeoutMs": 600_000,
            }},
        }))?;
        std::thread::sleep(after);
        self.notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": "relay acceptance cancels the call"}),
        )?;
        let until = Instant::now() + drain;
        while let Ok(line) = self
            .lines
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            let answered = serde_json::from_str::<Value>(&line)
                .ok()
                .is_some_and(|message| message.get("id").and_then(Value::as_u64) == Some(id));
            if answered {
                break;
            }
        }
        Ok(())
    }

    /// `relay_extract` of one declared output; answers where it landed.
    pub fn extract(&mut self, guest: &str, name: &str) -> Result<PathBuf, String> {
        let answer = self.call(
            "relay_extract",
            json!({"guest": guest, "names": [name]}),
            Duration::from_secs(60 * 60),
        )?;
        if answer.is_error {
            return Err(format!("relay_extract refused: {:?}", answer.parts));
        }
        answer
            .parts
            .iter()
            .flat_map(|part| match part {
                Value::Array(items) => items.clone(),
                Value::Object(map) => map
                    .get("extractions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| vec![part.clone()]),
                _ => Vec::new(),
            })
            .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|item| {
                item.get("destination")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            })
            .ok_or_else(|| format!("relay_extract named no destination: {:?}", answer.parts))
    }

    /// `relay_release` of the guest.
    pub fn release(&mut self, guest: &str) -> Result<(), String> {
        let answer = self.call(
            "relay_release",
            json!({"guest": guest}),
            Duration::from_secs(10 * 60),
        )?;
        if answer.is_error {
            return Err(format!("relay_release refused: {:?}", answer.parts));
        }
        Ok(())
    }

    /// The server's log so far: mcp-relay's session losses and resumes.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The server's log file.
    pub fn log_path(&self) -> &Path {
        &self.log
    }
}

impl Drop for McpRelay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a check does on a lease: run a command and read its exit and output.
/// The suite's own client and an mcp-relay guest both do it, so a check
/// written for one runs on the other.
pub trait Actions: Send + Sync {
    /// Run a command line within `timeout_s`, waiting at most `wait`.
    fn exec(&self, command: &str, timeout_s: f64, wait: Duration) -> Result<Answer, String>;

    /// Start a command line and cancel it after `after`, as the client
    /// cancels its call; answers what the cancel reported, when anything did.
    fn cancel_after(&self, command: &str, after: Duration) -> Result<String, String>;

    /// Run a command line, fail unless it exits 0, and return its output.
    fn run(&self, command: &str) -> Result<String, String> {
        let answer = self.exec(command, 600.0, Duration::from_secs(900))?;
        match answer.rc()? {
            Some(0) => Ok(answer.text()),
            rc => Err(format!(
                "`{command}` exited {rc:?}: {}",
                answer.text().trim()
            )),
        }
    }
}

impl Actions for RelayClient {
    fn exec(&self, command: &str, timeout_s: f64, wait: Duration) -> Result<Answer, String> {
        RelayClient::exec(self, command, timeout_s, wait)
    }

    fn cancel_after(&self, command: &str, after: Duration) -> Result<String, String> {
        let target = new_id("cancel-target");
        let running = self.call(&target, exec_op(command, false, 600.0), b"", true)?;
        std::thread::sleep(after);
        let cancel = self.call(&new_id("cancel"), Op::Cancel { target }, b"", true)?;
        let cancel = self.wait(cancel, Duration::from_secs(300))?;
        let ran = self.wait(running, Duration::from_secs(300))?;
        Ok(format!(
            "cancel {:?}, command {:?}",
            cancel.outcome(),
            ran.outcome()
        ))
    }
}

/// One guest that mcp-relay acquired, which the suite drives through
/// `relay_script`.
pub struct McpGuest {
    relay: Mutex<McpRelay>,
    log: PathBuf,
    /// The guest's name in mcp-relay's session.
    pub guest: String,
}

impl McpGuest {
    /// A guest of `relay`.
    pub fn new(relay: McpRelay, guest: String) -> McpGuest {
        McpGuest {
            log: relay.log_path().to_path_buf(),
            relay: Mutex::new(relay),
            guest,
        }
    }

    /// Release the guest through mcp-relay.
    pub fn release(&self) -> Result<(), String> {
        self.relay
            .lock()
            .map_err(|_| "mcp-relay is poisoned")?
            .release(&self.guest)
    }

    /// Bring a declared output home through mcp-relay.
    pub fn extract(&self, name: &str) -> Result<PathBuf, String> {
        self.relay
            .lock()
            .map_err(|_| "mcp-relay is poisoned")?
            .extract(&self.guest, name)
    }

    /// mcp-relay's log file, which a check may watch while a call runs.
    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// mcp-relay's log so far.
    pub fn log(&self) -> String {
        self.relay
            .lock()
            .map(|relay| relay.log())
            .unwrap_or_default()
    }
}

impl Actions for McpGuest {
    fn exec(&self, command: &str, timeout_s: f64, _wait: Duration) -> Result<Answer, String> {
        let timeout = Duration::from_secs_f64(timeout_s.max(1.0));
        self.relay
            .lock()
            .map_err(|_| "mcp-relay is poisoned")?
            .script(&self.guest, command, timeout)
    }

    fn cancel_after(&self, command: &str, after: Duration) -> Result<String, String> {
        self.relay
            .lock()
            .map_err(|_| "mcp-relay is poisoned")?
            .script_cancelled(&self.guest, command, after, Duration::from_secs(10))?;
        Ok("cancelled through MCP's notifications/cancelled".into())
    }
}

/// What `relay_acquire` leases.
#[derive(Clone, Copy, Debug)]
pub enum Acquire<'a> {
    /// A fresh VM of an image.
    Image(&'a str),
    /// A durable machine, by name.
    Machine(&'a str),
}

/// The JSON values one text part holds, in order: mcp-relay may put several
/// in one part, such as an image's delivery and then the run's result. A part
/// that is not JSON is kept as a string.
fn json_values(text: &str) -> Vec<Value> {
    let values: Result<Vec<Value>, _> = serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .collect();
    match values {
        Ok(values) if !values.is_empty() => values,
        _ => vec![Value::String(text.to_string())],
    }
}

/// A `relay_script` result as an [`Answer`]: an exit with the script's status,
/// and its standard output followed by its standard error as the kept bytes.
pub fn script_answer(answer: &ToolAnswer, elapsed: Duration) -> Result<Answer, String> {
    let result = answer
        .with("outcome")
        .ok_or_else(|| format!("relay_script gave no outcome: {:?}", answer.parts))?;
    let outcome = &result["outcome"];
    let text = format!(
        "{}{}",
        result
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        result
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    let bytes = text.into_bytes();
    let digest = Digest {
        size: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    };
    // A script past its timeout is `uncertain` with `timedOut` and its
    // termination confirmed: it was killed, so it is an exit that timed out.
    let timed_out = result
        .get("timedOut")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let confirmed = result
        .get("terminationConfirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rc = match outcome.get("kind").and_then(Value::as_str) {
        Some("completed") => outcome
            .pointer("/exitStatus/code")
            .and_then(Value::as_i64)
            .map(|code| code as i32),
        Some("uncertain") if timed_out && confirmed => None,
        Some(other) => return Err(format!("relay_script ended {other}: {outcome}")),
        None => return Err(format!("relay_script outcome has no kind: {outcome}")),
    };
    let signal = outcome
        .pointer("/exitStatus/signal")
        .and_then(Value::as_i64)
        .map(|s| s as i32);
    Ok(Answer {
        size: bytes.len() as u64,
        sha256: digest.sha256.clone(),
        bytes,
        result: Some(CallResult {
            call: 0,
            execution: result
                .get("executionId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            replayed: false,
            outcome: Outcome::Exit {
                rc,
                signal,
                timed_out,
                cancelled: false,
                output: digest,
            },
        }),
        elapsed,
        ..Answer::default()
    })
}

/// The session events mcp-relay logged: each `relay: session lost` and
/// `relay: session resumed` line, in order (mcp-relay DC-85).
pub fn session_events(log: &str) -> Vec<String> {
    log.lines()
        .filter(|line| line.starts_with("relay: session "))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_result_becomes_an_answer_with_its_exit_and_output() {
        let answer = ToolAnswer {
            is_error: true,
            parts: vec![
                json!({"imageDelivery": {"status": "pending"}}),
                json!({"executionId": "execution-1", "outcome": {"kind": "completed", "exitStatus": {"code": 3, "signal": null}}, "stdout": "out\n", "stderr": "err\n", "timedOut": false}),
            ],
        };
        let answer = script_answer(&answer, Duration::from_secs(1)).unwrap();
        assert_eq!(answer.rc().unwrap(), Some(3));
        assert_eq!(answer.text(), "out\nerr\n");
        assert_eq!(answer.result.unwrap().execution, "execution-1");
    }

    #[test]
    fn a_script_killed_at_its_timeout_is_an_exit_that_timed_out() {
        let answer = ToolAnswer {
            is_error: true,
            parts: vec![
                json!({"outcome": {"kind": "uncertain", "diagnostic": "execution timed out"}, "timedOut": true, "terminationConfirmed": true, "stdout": ""}),
            ],
        };
        let answer = script_answer(&answer, Duration::ZERO).unwrap();
        assert!(matches!(
            answer.outcome().unwrap(),
            Outcome::Exit {
                timed_out: true,
                rc: None,
                ..
            }
        ));
    }

    #[test]
    fn an_uncertain_script_is_an_error_not_an_exit() {
        let answer = ToolAnswer {
            is_error: true,
            parts: vec![
                json!({"outcome": {"kind": "uncertain", "diagnostic": "execution timed out"}}),
            ],
        };
        assert!(script_answer(&answer, Duration::ZERO)
            .unwrap_err()
            .contains("uncertain"));
    }

    #[test]
    fn a_part_with_two_json_values_yields_both() {
        let values =
            json_values("{\"imageDelivery\":{}}\n{\n  \"outcome\": {\"kind\": \"completed\"}\n}");
        assert_eq!(values.len(), 2);
        assert!(values[1].get("outcome").is_some());
        assert_eq!(
            json_values("plain words"),
            [Value::String("plain words".into())]
        );
    }

    #[test]
    fn session_events_are_the_relay_session_lines() {
        let log = "starting\nrelay: session lost: closed (1006)\nother\nrelay: session resumed\n";
        assert_eq!(
            session_events(log),
            [
                "relay: session lost: closed (1006)",
                "relay: session resumed"
            ]
        );
    }
}
