//! SSH and SCP helpers bound to a single lease identity.

use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{OpError, OpResult};
use crate::proc::{code_and_text, run_capture};

/// Build the `ssh` argument vector for a lease identity.
pub(crate) fn ssh_argv(
    ip: &str,
    user: &str,
    key_dir: &Path,
    remote_cmd: &str,
) -> OpResult<Vec<String>> {
    let mut argv = vec!["ssh".to_string()];
    argv.extend(lease_keys::key_args(key_dir).map_err(lease_error)?);
    argv.push("-T".to_string());
    argv.push(format!("{user}@{ip}"));
    argv.push(remote_cmd.to_string());
    Ok(argv)
}

fn lease_error(error: lease_keys::LeaseKeyError) -> OpError {
    OpError::new(error.to_string())
}

/// Run a remote command. Returns `(rc, combined_output)`, or `None` on a
/// connection or process failure, matching the Python `_ssh` helper.
pub fn ssh(
    _config: &Config,
    ip: &str,
    user: &str,
    key_dir: &Path,
    remote_cmd: &str,
    stdin: Option<Vec<u8>>,
    timeout_s: u64,
) -> Option<(i32, String)> {
    let argv = ssh_argv(ip, user, key_dir, remote_cmd).ok()?;
    let mut command = service_layout::tools::command_for(&argv[0]).ok()?;
    command.args(&argv[1..]);
    let output = run_capture(&mut command, stdin, Duration::from_secs(timeout_s)).ok()?;
    Some(code_and_text(&output))
}

/// Build the `scp` argument vector for a lease identity.
///
/// The operands are terminated with `--` so a path that looks like an option
/// (for example a leading `-`) is never parsed as one, matching the Python
/// `_scp` composition exactly.
fn scp_argv(key_dir: &Path, src: &str, dst: &str) -> OpResult<Vec<String>> {
    let mut argv: Vec<String> = vec!["scp".to_string(), "-r".to_string()];
    argv.extend(lease_keys::key_args(key_dir).map_err(lease_error)?);
    argv.push("--".to_string());
    argv.push(src.to_string());
    argv.push(dst.to_string());
    Ok(argv)
}

/// Copy one path with `scp -r`. Returns `(rc, combined_output)`, or `None`.
pub fn scp(
    _config: &Config,
    _ip: &str,
    _user: &str,
    key_dir: &Path,
    src: &str,
    dst: &str,
    timeout_s: u64,
) -> Option<(i32, String)> {
    let argv = scp_argv(key_dir, src, dst).ok()?;
    let mut command = service_layout::tools::command_for(&argv[0]).ok()?;
    command.args(&argv[1..]);
    let output = run_capture(&mut command, None, Duration::from_secs(timeout_s)).ok()?;
    Some(code_and_text(&output))
}

/// Verify a fresh key-only command connection, retrying only the harmless probe.
pub fn wait_ssh(config: &Config, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    loop {
        let probe_timeout = if timeout_s == 0 {
            20
        } else {
            timeout_s.min(20)
        };
        if let Some((0, _)) = ssh(config, ip, user, key_dir, "true", None, probe_timeout) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(Duration::from_secs(3)));
    }
}

/// Prove separate key-only push and pull connections and byte integrity.
pub fn verify_transfer(
    config: &Config,
    ip: &str,
    user: &str,
    key_dir: &Path,
    state_dir: &Path,
) -> OpResult<()> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let remote = format!(
        "/var/tmp/{}-vm-service-key-probe-{}",
        utc_timestamp_compact(),
        nonce
    );
    let temp = tempfile::Builder::new()
        .prefix("key-probe-")
        .tempdir_in(state_dir)
        .map_err(OpError::from)?;
    let source = temp.path().join("source");
    let destination = temp.path().join("returned");
    let content = random_bytes(4096);
    std::fs::write(&source, &content)?;
    let result = (|| -> OpResult<()> {
        let sent = scp(
            config,
            ip,
            user,
            key_dir,
            &source.to_string_lossy(),
            &format!("{user}@{ip}:{remote}"),
            30,
        );
        if sent.as_ref().map(|(rc, _)| *rc) != Some(0) {
            return Err(OpError::new("Key-only readiness upload failed"));
        }
        let received = scp(
            config,
            ip,
            user,
            key_dir,
            &format!("{user}@{ip}:{remote}"),
            &destination.to_string_lossy(),
            30,
        );
        let downloaded = received.as_ref().map(|(rc, _)| *rc) == Some(0);
        let matches = std::fs::read(&destination)
            .map(|bytes| Sha256::digest(&bytes) == Sha256::digest(&content))
            .unwrap_or(false);
        if !downloaded || !matches {
            return Err(OpError::new(
                "Key-only readiness download or checksum failed",
            ));
        }
        Ok(())
    })();
    let removed = ssh(
        config,
        ip,
        user,
        key_dir,
        &format!("rm -f -- {}", shell_quote(&remote)),
        None,
        20,
    );
    if result.is_ok() && removed.as_ref().map(|(rc, _)| *rc) != Some(0) {
        return Err(OpError::new("Key-only readiness probe cleanup failed"));
    }
    result
}

/// Quote a string for a POSIX shell, matching Python `shlex.quote`.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_-+=:,./".contains(c))
    {
        return value.to_string();
    }
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn random_bytes(count: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buffer = vec![0u8; count];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut buffer).is_ok() {
            return buffer;
        }
    }
    // Last-resort fallback; never used when /dev/urandom is available.
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = (uuid::Uuid::new_v4().as_bytes()[index % 16]).wrapping_add(index as u8);
    }
    buffer
}

/// Format the current UTC time as `%Y-%m-%d-%H-%M-%S-Z`.
pub fn utc_timestamp_compact() -> String {
    // SAFETY: `gmtime_r` writes into a caller-provided `tm`.
    unsafe {
        let now: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&now, &mut tm).is_null() {
            return "1970-01-01-00-00-00-Z".to_string();
        }
        format!(
            "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}-Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, validated lease key directory, exactly as `lease_keys::create`
    /// produces it in production.
    fn lease_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let state = std::fs::canonicalize(dir.path()).expect("canonical state");
        let path = lease_keys::create(&state, "vm-one").expect("lease keys");
        (dir, path)
    }

    /// `test_scp_uses_the_same_explicit_identity_and_host_pin`: the `scp -r`
    /// argv carries the lease identity and strict host-key pinning, and
    /// terminates its operands with `--`.
    #[test]
    fn scp_argv_is_strict_and_terminates_operands() {
        let (_tmp, path) = lease_path();
        let src = "/tmp/source";
        let dst = "admin@127.0.0.1:/tmp/target";
        let argv = scp_argv(&path, src, dst).expect("scp argv");
        assert_eq!(&argv[..2], ["scp", "-r"]);
        let identity = path.join("identity").to_string_lossy().into_owned();
        assert!(
            argv.iter().any(|arg| arg == &identity),
            "missing lease identity: {argv:?}"
        );
        for option in [
            "StrictHostKeyChecking=yes",
            "KbdInteractiveAuthentication=no",
        ] {
            assert!(argv.iter().any(|arg| arg == option), "missing {option}");
        }
        let known_hosts = format!(
            "UserKnownHostsFile=\"{}\"",
            path.join("known_hosts").display()
        );
        assert!(
            argv.iter().any(|arg| arg == &known_hosts),
            "missing {known_hosts}"
        );
        let separator = argv
            .iter()
            .position(|arg| arg == "--")
            .expect("-- operand terminator");
        assert_eq!(&argv[separator + 1..], [src, dst]);
        assert!(
            !argv.iter().any(|arg| arg.contains("sshpass")),
            "scp must never use sshpass: {argv:?}"
        );
    }

    /// `test_exec_uses_key_only_and_returns_remote_255_without_replay`: the
    /// `ssh` argv is key-only (`-i`, `-T`), pins the first-use host key, and
    /// never contains `sshpass`.
    #[test]
    fn ssh_argv_is_key_only_and_never_sshpass() {
        let (_tmp, path) = lease_path();
        let argv = ssh_argv("127.0.0.1", "admin", &path, "exit 255").expect("ssh argv");
        assert_eq!(argv[0], "ssh");
        assert!(argv.iter().any(|arg| arg == "-T"));
        assert!(
            argv.iter().any(|arg| arg == "StrictHostKeyChecking=yes"),
            "missing host-key pin: {argv:?}"
        );
        let identity = path.join("identity").to_string_lossy().into_owned();
        assert!(
            argv.iter().any(|arg| arg == &identity),
            "missing lease identity: {argv:?}"
        );
        assert_eq!(argv[argv.len() - 2], "admin@127.0.0.1");
        assert_eq!(argv[argv.len() - 1], "exit 255");
        assert!(
            !argv.iter().any(|arg| arg.contains("sshpass")),
            "ssh must never use sshpass: {argv:?}"
        );
    }
}
