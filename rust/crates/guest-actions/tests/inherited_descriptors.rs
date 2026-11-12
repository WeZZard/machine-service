//! A job under the fake launchctl and what it leaves running keep every
//! descriptor its caller held, out of the way of the prelude's own, so a
//! test still knows them as its own (RA-10, DC-254). A test binary of its
//! own, because it puts a pipe at descriptor 9 of the whole process: the
//! number the prelude reports on.

#![cfg(target_os = "macos")]

use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use guest_actions::launchd::{label_for, EnvWriter, Launchd};
use guest_actions::spawn_job;

/// The kernel's address of the pipe end at `fd` of `pid`, as `lsof` names it.
fn pipe_ends(pid: i32) -> Vec<String> {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-p", &pid.to_string(), "-a", "-d", "0-999", "-F", "ftd"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut ends = Vec::new();
    let mut pipe = false;
    for line in text.lines() {
        if let Some(kind) = line.strip_prefix('t') {
            pipe = kind == "PIPE";
        } else if let Some(device) = line.strip_prefix('d') {
            if pipe {
                ends.push(device.to_string());
            }
        }
    }
    ends
}

#[test]
fn a_jobs_leftover_still_holds_a_pipe_its_caller_had_at_descriptor_9() {
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../service-layout/tests/fixtures/launchd/launchctl")
        .canonicalize()
        .unwrap();
    std::os::unix::fs::symlink(&fake, tools.join("launchctl")).unwrap();
    // SAFETY: this test is the only one in its process.
    unsafe {
        std::env::set_var(service_layout::tools::TEST_VARIABLE, &tools);
        std::env::set_var("FAKE_LAUNCHD_STATE", root.path().join("state"));
    }
    let launchd = Launchd::resolve().unwrap();

    // A pipe whose read end sits at descriptor 9, inherited by children,
    // as a test harness's lifeline may.
    let (reader, writer) = std::io::pipe().unwrap();
    // SAFETY: dup2 onto 9, which this test process does not otherwise use.
    assert_eq!(unsafe { libc::dup2(reader.as_raw_fd(), 9) }, 9);
    let ours = pipe_ends(std::process::id() as i32);
    drop(reader);

    let environment: Vec<(OsString, OsString)> = Vec::new();
    let spawned = spawn_job(
        &launchd,
        &label_for("fd9", "k"),
        Path::new("/bin/zsh"),
        &root.path().join("job"),
        "nohup sleep 30 >/dev/null 2>&1 & echo $!",
        false,
        EnvWriter::Here {
            environment: &environment,
            cwd: root.path(),
        },
    )
    .unwrap();
    let mut output = Vec::new();
    let exit = spawned.supervise_from(None, Duration::from_secs(20), |bytes| {
        output.extend_from_slice(bytes);
        true
    });
    assert_eq!(exit.rc, Some(0), "{}", String::from_utf8_lossy(&output));
    let leftover: i32 = String::from_utf8_lossy(&output).trim().parse().unwrap();
    let held = pipe_ends(leftover);
    // SAFETY: plain kill of the process this test started.
    unsafe { libc::kill(leftover, libc::SIGKILL) };
    drop(writer);
    assert!(
        held.iter().any(|end| ours.contains(end)),
        "the leftover holds none of the caller's pipe ends: {held:?}, ours {ours:?}"
    );
}
