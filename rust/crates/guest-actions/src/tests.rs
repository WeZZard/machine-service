//! TL-1 unit tests of the shared actions.

use std::path::Path;
use std::time::Duration;

use super::*;

fn run(home: &Path, command: &str, stdin: Option<&[u8]>, timeout: Duration) -> (Exit, Vec<u8>) {
    let spawned = spawn_shell(Path::new("/bin/sh"), home, command, stdin.is_some()).unwrap();
    let mut output = Vec::new();
    let exit = spawned.supervise(stdin.map(<[u8]>::to_vec), timeout, |bytes| {
        output.extend_from_slice(bytes);
        true
    });
    (exit, output)
}

#[test]
fn a_command_runs_through_the_named_shell_in_home() {
    let home = tempfile::tempdir().unwrap();
    let (exit, output) = run(
        home.path(),
        "echo $0; pwd; exit 3",
        None,
        Duration::from_secs(10),
    );
    assert_eq!(exit.rc, Some(3));
    let text = String::from_utf8(output).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("sh"));
    let pwd = std::fs::canonicalize(lines.next().unwrap()).unwrap();
    assert_eq!(pwd, std::fs::canonicalize(home.path()).unwrap());
}

#[test]
fn standard_input_reaches_the_command() {
    let home = tempfile::tempdir().unwrap();
    let (exit, output) = run(home.path(), "cat", Some(b"abc"), Duration::from_secs(10));
    assert_eq!((exit.rc, output), (Some(0), b"abc".to_vec()));
}

#[test]
fn a_timeout_kills_the_group() {
    let home = tempfile::tempdir().unwrap();
    let (exit, _) = run(
        home.path(),
        "sleep 30 & wait",
        None,
        Duration::from_millis(200),
    );
    assert!(exit.timed_out);
    assert_eq!(exit.signal, Some(libc::SIGKILL));
}

#[test]
fn an_atomic_file_appears_only_when_renamed() {
    let home = tempfile::tempdir().unwrap();
    let mut file = AtomicFile::create("out.txt", "", Some(0o640), home.path(), "t", 1).unwrap();
    file.append(b"hello").unwrap();
    assert!(!home.path().join("out.txt").exists());
    let sealed = file.seal(5, &sha256_hex(b"hello")).unwrap();
    assert!(!home.path().join("out.txt").exists());
    let digest = sealed.rename().unwrap();
    assert_eq!(digest.size, 5);
    let metadata = std::fs::metadata(home.path().join("out.txt")).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
}

#[test]
fn an_abandoned_or_mismatched_write_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let mut file = AtomicFile::create("a", "", None, home.path(), "t", 1).unwrap();
    file.append(b"x").unwrap();
    drop(file);
    let mut file = AtomicFile::create("b", "", None, home.path(), "t", 2).unwrap();
    file.append(b"x").unwrap();
    let error = file.seal(1, &sha256_hex(b"y")).err().unwrap();
    assert_eq!(error.code, "checksum_mismatch");
    let sealed = AtomicFile::create("c", "", None, home.path(), "t", 3)
        .unwrap()
        .seal(0, &sha256_hex(b""))
        .unwrap();
    drop(sealed);
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn reads_classify_directories_and_missing_files() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(open_read(home.path()).err().unwrap().code, "is_directory");
    assert_eq!(
        open_read(&home.path().join("nope")).err().unwrap().code,
        "not_found"
    );
}

#[test]
fn read_chunks_respects_the_chunk_size() {
    let bytes = vec![7u8; 10_000];
    let mut sizes = Vec::new();
    let digest = read_chunks(&bytes[..], 4096, |chunk| {
        sizes.push(chunk.len());
        true
    })
    .unwrap();
    assert!(sizes.iter().all(|size| *size <= 4096));
    assert_eq!(digest.size, 10_000);
    assert_eq!(digest.sha256, sha256_hex(&bytes));
}

#[test]
fn resolve_follows_scp() {
    let home = Path::new("/h");
    assert_eq!(resolve("~", home), Path::new("/h"));
    assert_eq!(resolve("~/a", home), Path::new("/h/a"));
    assert_eq!(resolve("a/b", home), Path::new("/h/a/b"));
    assert_eq!(resolve("/abs", home), Path::new("/abs"));
}

// ------------------------------------------------------------- ownership

/// A process of its own group that sleeps, as an unrelated process would.
fn sleeper() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn reap(mut child: std::process::Child) -> std::process::ExitStatus {
    child.wait().unwrap()
}

#[test]
fn dc139_a_start_time_names_one_process() {
    let own = ownership::process_start(std::process::id() as i32).expect("own start time");
    assert_eq!(
        ownership::process_start(std::process::id() as i32).as_deref(),
        Some(own.as_str()),
        "stable for one process"
    );
    // Linux counts start times in clock ticks (10 ms), so a process started
    // in the same tick has the same start time; a record matches a process
    // ID and its start time together, and no ID is reused within a tick.
    std::thread::sleep(Duration::from_millis(50));
    let child = sleeper();
    let pid = child.id() as i32;
    let other = ownership::process_start(pid).expect("the child's start time");
    assert_ne!(other, own, "another process has another start time");
    kill_group(pid);
    reap(child);
    assert_eq!(
        ownership::process_start(pid),
        None,
        "a gone process has none"
    );
    assert_eq!(ownership::process_start(0), None);
    assert_eq!(ownership::process_start(-1), None);
}

#[test]
fn dc139_a_reused_process_id_is_never_killed() {
    // The record names the group of a process that has the ID now but
    // started at another time: an unrelated process that reused the ID.
    let child = sleeper();
    let pid = child.id() as i32;
    let started = ownership::process_start(pid).unwrap();
    let stale = format!("{started}-earlier");
    assert_eq!(
        ownership::stop_group(pid, &stale),
        ownership::Stopped::Reused
    );
    assert_eq!(
        ownership::Owner::Group {
            group: pid,
            started: String::new(),
        }
        .stop_leftover(),
        ownership::Stopped::Reused,
        "an unknown start time matches nothing"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(alive(pid), "the unrelated process still runs");

    // The same record with the start time it had is the command's group.
    assert_eq!(
        ownership::Owner::Group {
            group: pid,
            started,
        }
        .stop_leftover(),
        ownership::Stopped::Killed
    );
    let status = reap(child);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL)
    );
    assert_eq!(ownership::stop_group(pid, "x"), ownership::Stopped::Gone);
}

#[test]
fn dc139_ownership_names_a_scope_per_command_or_says_why_not() {
    let scopes = ownership::Ownership::Scopes;
    let first = scopes.scope_for("abc").unwrap();
    assert!(
        first.starts_with("relay-agent-abc-") && first.ends_with(".scope"),
        "{first}"
    );
    assert_ne!(
        scopes.scope_for("abc").unwrap(),
        first,
        "unique per command"
    );
    assert_eq!(scopes.describe(), "scope");
    let groups = ownership::Ownership::Groups {
        reason: "why".into(),
    };
    assert_eq!(groups.scope_for("abc"), None);
    assert_eq!(groups.describe(), "process-group (why)");
    let detected = ownership::Ownership::detect();
    if cfg!(target_os = "macos") {
        // RA-10: on macOS every command is a launchd job of its own.
        assert_eq!(detected, ownership::Ownership::Jobs);
        assert_eq!(detected.scope_for("abc"), None);
        assert_eq!(detected.describe(), "launchd-job");
    }
    // Records survive encoding.
    for owner in [
        ownership::Owner::Job {
            label: "com.wezzard.relay-agent.job.a.k.1-2-3".into(),
        },
        ownership::Owner::Scope {
            unit: "relay-agent-k.scope".into(),
        },
        ownership::Owner::Group {
            group: 42,
            started: "1.000001".into(),
        },
    ] {
        let text = serde_json::to_string(&owner).unwrap();
        assert_eq!(
            serde_json::from_str::<ownership::Owner>(&text).unwrap(),
            owner
        );
    }
}

// ------------------------------------------------------------------ trees

fn tree_stream(source: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    let digest = read_tree(source, 7, |piece| {
        assert!(piece.len() <= 7);
        bytes.extend_from_slice(piece);
        true
    })
    .unwrap();
    assert_eq!(digest.size, bytes.len() as u64);
    assert_eq!(digest.sha256, sha256_hex(&bytes));
    bytes
}

#[test]
fn a_folder_round_trips_through_its_tar_stream_and_lands_whole() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("a.txt"), "alpha").unwrap();
    std::fs::write(source.join("nested/b.bin"), [0u8, 1, 2]).unwrap();
    let bytes = tree_stream(&source);

    // Into an existing folder, under the given name, as `scp -r` does.
    std::fs::create_dir(home.path().join("into")).unwrap();
    let mut tree = AtomicTree::create("into", "copy", home.path(), "t", 1).unwrap();
    assert_eq!(tree.destination(), home.path().join("into/copy"));
    tree.append(&bytes).unwrap();
    let sealed = tree.seal(bytes.len() as u64, &sha256_hex(&bytes)).unwrap();
    assert!(
        !home.path().join("into/copy").exists(),
        "nothing is visible before the placement"
    );
    sealed.place().unwrap();
    assert_eq!(
        std::fs::read_to_string(home.path().join("into/copy/a.txt")).unwrap(),
        "alpha"
    );

    // Over an existing folder: replaced whole, nothing left beside it.
    std::fs::write(home.path().join("into/copy/stale"), "old").unwrap();
    let mut tree = AtomicTree::create("into/copy", "", home.path(), "t", 2);
    assert!(tree.is_err(), "a folder destination needs a name");
    tree = AtomicTree::create("~/into", "copy", home.path(), "t", 2);
    let mut tree = tree.unwrap();
    tree.append(&bytes).unwrap();
    tree.seal(bytes.len() as u64, &sha256_hex(&bytes))
        .unwrap()
        .place()
        .unwrap();
    assert!(!home.path().join("into/copy/stale").exists());
    let left: Vec<_> = std::fs::read_dir(home.path().join("into"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["copy"], "no temporary is left");
}

#[test]
fn a_folder_write_that_fails_its_check_leaves_nothing() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("f"), "x").unwrap();
    let bytes = tree_stream(&source);
    let mut tree = AtomicTree::create("dest", "", home.path(), "t", 1).unwrap();
    tree.append(&bytes).unwrap();
    let error = tree
        .seal(bytes.len() as u64, &sha256_hex(b"other"))
        .err()
        .unwrap();
    assert_eq!(error.code, "checksum_mismatch");
    let mut tree = AtomicTree::create("dest", "", home.path(), "t", 2).unwrap();
    tree.append(b"not a tar stream at all, but long enough to be read")
        .unwrap();
    let garbage = b"not a tar stream at all, but long enough to be read";
    let sealed = tree.seal(garbage.len() as u64, &sha256_hex(garbage));
    assert!(sealed.is_err());
    let names: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["source"]);
    std::fs::write(home.path().join("file"), "x").unwrap();
    assert_eq!(
        AtomicTree::create("file", "", home.path(), "t", 3)
            .err()
            .unwrap()
            .code,
        "not_a_directory"
    );
}

#[test]
fn a_folder_read_refuses_files_missing_paths_and_links_before_any_byte() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("file"), "x").unwrap();
    let mut sent = 0;
    let mut count = |_: &[u8]| {
        sent += 1;
        true
    };
    assert_eq!(
        read_tree(&home.path().join("file"), 64, &mut count)
            .err()
            .unwrap()
            .code,
        "not_a_directory"
    );
    assert_eq!(
        read_tree(&home.path().join("none"), 64, &mut count)
            .err()
            .unwrap()
            .code,
        "not_found"
    );
    let linked = home.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", linked.join("link")).unwrap();
    assert_eq!(
        read_tree(&linked, 64, &mut count).err().unwrap().code,
        "unsupported_entry"
    );
    assert_eq!(sent, 0);
}
