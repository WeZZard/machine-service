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
