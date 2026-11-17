//! `scripts/fetch-relay-agent-release.sh`, which a deploy's build runs to
//! take the published relay agent release (DC-356), refuses anything but
//! the signed release. The tests serve a release through a stand-in `gh`
//! first on `PATH`; the accepted path, which needs a real Developer ID
//! signature, is checked against the published release by hand.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const FAKE_GH: &str = r#"#!/bin/sh
# Stand-in gh: `release view <tag> --json isDraft --jq .isDraft` prints
# $FAKE_GH_DRAFT; `release download <tag> --dir D ...` copies $FAKE_GH_RELEASE.
case "$1 $2" in
"release view") echo "$FAKE_GH_DRAFT" ;;
"release download")
    shift 3
    while [ $# -gt 0 ]; do
        if [ "$1" = --dir ]; then dir=$2; fi
        shift
    done
    cp "$FAKE_GH_RELEASE"/* "$dir/"
    ;;
*) exit 99 ;;
esac
"#;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("scripts/fetch-relay-agent-release.sh")
}

/// A release of `version` in `folder`: its macOS archive holds a copy of a
/// system executable signed ad hoc with the release identifier, and its
/// SIGNING.json records `signature` and that executable's SHA-256.
fn write_release(folder: &Path, version: &str, signature: &str) {
    fs::create_dir_all(folder).unwrap();
    let stage = tempfile::tempdir().unwrap();
    let member = stage.path().join("relay-agent");
    fs::copy("/usr/bin/true", &member).unwrap();
    let signed = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            "com.wezzard.relay-agent",
        ])
        .arg(&member)
        .output()
        .unwrap();
    assert!(signed.status.success());
    let file = format!("relay-agent-{version}-darwin-arm64.tar.gz");
    let status = Command::new("/usr/bin/tar")
        .arg("-C")
        .arg(stage.path())
        .arg("-czf")
        .arg(folder.join(&file))
        .arg("relay-agent")
        .status()
        .unwrap();
    assert!(status.success());
    let sha = |path: &Path| service_layout::release::sha256_hex(&fs::read(path).unwrap());
    fs::write(
        folder.join("SHA256SUMS"),
        format!("{}  {file}\n", sha(&folder.join(&file))),
    )
    .unwrap();
    fs::write(
        folder.join("SIGNING.json"),
        format!(
            "{{\n \"schemaVersion\": 1,\n \"version\": \"{version}\",\n \"platform\": \"darwin-arm64\",\n \"signature\": \"{signature}\",\n \"awaitsSigning\": false,\n \"executableSha256\": \"{}\"\n}}\n",
            sha(&member)
        ),
    )
    .unwrap();
}

struct Fetch {
    _root: tempfile::TempDir,
    bin: PathBuf,
    release: PathBuf,
    dest: PathBuf,
}

impl Fetch {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, FAKE_GH).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let release = root.path().join("published");
        let dest = root.path().join("dest");
        Fetch {
            bin,
            release,
            dest,
            _root: root,
        }
    }

    fn run(&self, tag: &str, draft: bool) -> (bool, String) {
        let output = Command::new("/bin/sh")
            .arg(script())
            .arg(tag)
            .arg(&self.dest)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("FAKE_GH_DRAFT", if draft { "true" } else { "false" })
            .env("FAKE_GH_RELEASE", &self.release)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn refused(&self, tag: &str, draft: bool, why: &str) {
        let (passed, stderr) = self.run(tag, draft);
        assert!(!passed, "accepted: {stderr}");
        assert!(stderr.contains(why), "{stderr}");
        assert!(
            !self.dest.exists(),
            "a refusal wrote {}",
            self.dest.display()
        );
    }
}

#[test]
fn a_draft_is_refused() {
    let fetch = Fetch::new();
    write_release(&fetch.release, "1.2.3", "developer-id");
    fetch.refused("v1.2.3", true, "is a draft");
}

#[test]
fn an_ad_hoc_record_is_refused() {
    let fetch = Fetch::new();
    write_release(&fetch.release, "1.2.3", "adhoc");
    fetch.refused("v1.2.3", false, "not developer-id");
}

#[test]
fn another_version_is_refused() {
    let fetch = Fetch::new();
    write_release(&fetch.release, "1.2.4", "developer-id");
    fetch.refused("v1.2.3", false, "not an archive of version 1.2.3");
}

#[test]
fn a_tampered_archive_is_refused() {
    let fetch = Fetch::new();
    write_release(&fetch.release, "1.2.3", "developer-id");
    let archive = fetch.release.join("relay-agent-1.2.3-darwin-arm64.tar.gz");
    let mut bytes = fs::read(&archive).unwrap();
    bytes.push(0);
    fs::write(&archive, bytes).unwrap();
    fetch.refused("v1.2.3", false, "do not match its SHA256SUMS");
}

#[test]
fn an_executable_signed_ad_hoc_is_refused_whatever_the_record_says() {
    let fetch = Fetch::new();
    write_release(&fetch.release, "1.2.3", "developer-id");
    fetch.refused("v1.2.3", false, "not signed with Developer ID");
}
