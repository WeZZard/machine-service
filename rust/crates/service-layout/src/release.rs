//! The relay agent's release, installed beside the daemon (DC-140, IN-1).
//!
//! A published release is the only source of a relay agent that reaches a
//! machine: `scripts/build-relay-agent.sh` makes it, as the release workflow
//! does, and it is a folder of `relay-agent-<version>-<platform>.tar.gz`
//! archives, each holding the one executable `relay-agent`, with
//! `SHA256SUMS` over them. The installer copies that folder verbatim into
//! the install folder as [`FOLDER`]; the daemon uploads into a guest, and
//! the installer installs on a durable Mac, exactly the executable that the
//! archive for the platform holds, after checking the archive against
//! `SHA256SUMS`. Nothing else is a relay agent build.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

/// The release's folder beside the daemon.
pub const FOLDER: &str = "relay-agent-release";
/// The checksum list of a release, as `shasum -a 256` writes it.
pub const SUMS: &str = "SHA256SUMS";
/// The one member of every archive.
pub const MEMBER: &str = "relay-agent";
/// Files beside the daemon that earlier layouts installed as the relay
/// agent's own builds; an installation removes them, because nothing reads
/// them any more.
pub const RETIRED: [&str; 2] = ["relay-agent", "relay-agent-linux"];
/// The system `tar`, which reads the gzip archives on macOS and Linux.
const TAR: &str = "/usr/bin/tar";

/// This host's processor architecture as a release names it.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// The release platform that serves a guest of `kind` (`macos` or `linux`)
/// on this host, whose processor architecture every guest shares.
pub fn platform_for(kind: &str) -> String {
    let os = if kind == "linux" { "linux" } else { "darwin" };
    format!("{os}-{}", host_arch())
}

/// The platforms an installation needs: Linux and macOS guests when it
/// creates virtual machines, and macOS when it exposes its own Mac, whose
/// relay agent the installer installs (RA-2).
pub fn platforms(creates_virtual_machines: bool, exposed: bool) -> Vec<String> {
    let mut platforms = Vec::new();
    if creates_virtual_machines || exposed {
        platforms.push(platform_for("macos"));
    }
    if creates_virtual_machines {
        platforms.push(platform_for("linux"));
    }
    platforms
}

/// The release folder beside the running executable.
pub fn beside_daemon() -> Option<PathBuf> {
    crate::daemon_directory().map(|directory| directory.join(FOLDER))
}

/// One archive of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The platform, such as `darwin-arm64`.
    pub platform: String,
    /// The archive's file name in the release folder.
    pub file: String,
    /// Its SHA-256 from `SHA256SUMS`.
    pub sha256: String,
}

/// A release folder whose `SHA256SUMS` has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The folder.
    pub dir: PathBuf,
    /// The version every archive carries.
    pub version: String,
    /// The archives `SHA256SUMS` names, in its order.
    pub archives: Vec<Archive>,
}

/// The SHA-256 of `bytes`, in lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `relay-agent-<version>-<os>-<arch>.tar.gz` as `(version, platform)`.
fn parse_name(file: &str) -> Option<(String, String)> {
    let stem = file.strip_prefix("relay-agent-")?.strip_suffix(".tar.gz")?;
    let mut parts = stem.rsplitn(3, '-');
    let arch = parts.next()?;
    let os = parts.next()?;
    let version = parts.next()?;
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    };
    (plain(arch) && plain(os) && !version.is_empty() && !version.contains('/'))
        .then(|| (version.to_string(), format!("{os}-{arch}")))
}

impl Release {
    /// Read the release in `dir`: its `SHA256SUMS` must name at least one
    /// archive, each once per platform, all of one version.
    pub fn open(dir: &Path) -> Result<Release, String> {
        let sums = dir.join(SUMS);
        let text = std::fs::read_to_string(&sums)
            .map_err(|error| format!("no relay agent release at {}: {error}", dir.display()))?;
        let mut archives: Vec<Archive> = Vec::new();
        let mut version: Option<String> = None;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let malformed = || format!("{}: malformed line {line:?}", sums.display());
            let (sha256, file) = line.split_once(char::is_whitespace).ok_or_else(malformed)?;
            // `shasum` marks a binary-mode entry with `*`.
            let file = file.trim_start().trim_start_matches('*');
            if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(malformed());
            }
            let (this_version, platform) = parse_name(file).ok_or_else(|| {
                format!(
                    "{}: {file:?} is not relay-agent-<version>-<platform>.tar.gz",
                    sums.display()
                )
            })?;
            match &version {
                Some(seen) if *seen != this_version => {
                    return Err(format!(
                        "{}: archives of two versions, {seen} and {this_version}",
                        sums.display()
                    ))
                }
                _ => version = Some(this_version),
            }
            if archives.iter().any(|archive| archive.platform == platform) {
                return Err(format!("{}: two archives for {platform}", sums.display()));
            }
            archives.push(Archive {
                platform,
                file: file.to_string(),
                sha256: sha256.to_ascii_lowercase(),
            });
        }
        let version = version.ok_or_else(|| format!("{} names no archive", sums.display()))?;
        Ok(Release {
            dir: dir.to_path_buf(),
            version,
            archives,
        })
    }

    /// The archive for `platform`.
    pub fn archive(&self, platform: &str) -> Result<&Archive, String> {
        self.archives
            .iter()
            .find(|archive| archive.platform == platform)
            .ok_or_else(|| {
                format!(
                    "the relay agent release {} in {} has no archive for {platform}",
                    self.version,
                    self.dir.display()
                )
            })
    }

    /// The archive for `platform`, checked against `SHA256SUMS`.
    pub fn verified(&self, platform: &str) -> Result<PathBuf, String> {
        let archive = self.archive(platform)?;
        let path = self.dir.join(&archive.file);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let actual = sha256_hex(&bytes);
        if actual != archive.sha256 {
            return Err(format!(
                "{} does not match {SUMS}: its SHA-256 is {actual}, the release says {}",
                path.display(),
                archive.sha256
            ));
        }
        Ok(path)
    }

    /// The members of the archive for `platform`, which must be exactly
    /// [`MEMBER`].
    pub fn check_members(&self, platform: &str) -> Result<(), String> {
        let path = self.verified(platform)?;
        let listed = tar(&["-tzf"], &path, None)?;
        let members: Vec<String> = String::from_utf8_lossy(&listed)
            .lines()
            .map(|line| line.trim_start_matches("./").to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if members != [MEMBER] {
            return Err(format!(
                "{} must hold the one file {MEMBER}, and holds {members:?}",
                path.display()
            ));
        }
        Ok(())
    }

    /// The relay agent executable the archive for `platform` holds, after
    /// the archive is checked against `SHA256SUMS`.
    pub fn executable(&self, platform: &str) -> Result<Vec<u8>, String> {
        let path = self.verified(platform)?;
        let bytes = tar(&["-xzOf"], &path, Some(MEMBER))?;
        if bytes.is_empty() {
            return Err(format!("{} holds an empty {MEMBER}", path.display()));
        }
        Ok(bytes)
    }
}

fn tar(flags: &[&str], archive: &Path, member: Option<&str>) -> Result<Vec<u8>, String> {
    let mut command = Command::new(TAR);
    command.args(flags).arg(archive);
    if let Some(member) = member {
        command.arg(member);
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{TAR}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{TAR} {} {} failed: {}",
            flags.join(" "),
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release folder as `build-relay-agent.sh` makes it, with one archive
    /// per platform holding `relay-agent` with `content`.
    fn release(platforms: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut sums = String::new();
        for (platform, content) in platforms {
            let stage = tempfile::tempdir().unwrap();
            std::fs::write(stage.path().join(MEMBER), content).unwrap();
            let file = format!("relay-agent-1.2.3-{platform}.tar.gz");
            let status = Command::new(TAR)
                .arg("-C")
                .arg(stage.path())
                .arg("-czf")
                .arg(dir.path().join(&file))
                .arg(MEMBER)
                .status()
                .unwrap();
            assert!(status.success());
            let bytes = std::fs::read(dir.path().join(&file)).unwrap();
            sums.push_str(&format!("{}  {file}\n", sha256_hex(&bytes)));
        }
        std::fs::write(dir.path().join(SUMS), sums).unwrap();
        dir
    }

    #[test]
    fn names_parse_into_version_and_platform() {
        assert_eq!(
            parse_name("relay-agent-0.1.1-linux-x86_64.tar.gz"),
            Some(("0.1.1".into(), "linux-x86_64".into()))
        );
        assert_eq!(
            parse_name("relay-agent-0.2.0-rc1-darwin-arm64.tar.gz"),
            Some(("0.2.0-rc1".into(), "darwin-arm64".into()))
        );
        assert_eq!(parse_name("relay-agent-darwin-arm64.tar.gz"), None);
        assert_eq!(parse_name("other-0.1.1-darwin-arm64.tar.gz"), None);
        assert_eq!(parse_name("relay-agent-0.1.1-darwin-arm64.zip"), None);
    }

    #[test]
    fn platforms_follow_what_the_installation_serves() {
        let mac = platform_for("macos");
        let linux = platform_for("linux");
        assert!(mac.starts_with("darwin-") && linux.starts_with("linux-"));
        assert_eq!(platforms(true, false), [mac.clone(), linux.clone()]);
        assert_eq!(platforms(true, true), [mac.clone(), linux]);
        assert_eq!(platforms(false, true), [mac]);
        assert!(platforms(false, false).is_empty());
    }

    #[test]
    fn the_executable_is_the_archives_member() {
        let dir = release(&[("darwin-arm64", b"mach-o"), ("linux-arm64", b"elf")]);
        let release = Release::open(dir.path()).unwrap();
        assert_eq!(release.version, "1.2.3");
        assert_eq!(release.executable("darwin-arm64").unwrap(), b"mach-o");
        assert_eq!(release.executable("linux-arm64").unwrap(), b"elf");
        release.check_members("linux-arm64").unwrap();
        let missing = release.executable("linux-x86_64").unwrap_err();
        assert!(missing.contains("no archive for linux-x86_64"), "{missing}");
    }

    #[test]
    fn an_archive_that_does_not_match_its_sum_is_refused() {
        let dir = release(&[("darwin-arm64", b"mach-o")]);
        let release = Release::open(dir.path()).unwrap();
        let file = dir.path().join(&release.archives[0].file);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.push(0);
        std::fs::write(&file, bytes).unwrap();
        let error = release.executable("darwin-arm64").unwrap_err();
        assert!(error.contains("does not match SHA256SUMS"), "{error}");
    }

    #[test]
    fn a_malformed_or_mixed_release_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Release::open(dir.path())
            .unwrap_err()
            .contains("no relay agent release"));
        let sum = "a".repeat(64);
        for (text, expected) in [
            ("", "names no archive"),
            ("nonsense\n", "malformed"),
            (&*format!("{sum}  relay-agent.tar.gz\n"), "is not relay-agent-"),
            (
                &*format!(
                    "{sum}  relay-agent-1.0.0-darwin-arm64.tar.gz\n{sum}  relay-agent-1.0.1-linux-arm64.tar.gz\n"
                ),
                "two versions",
            ),
            (
                &*format!(
                    "{sum}  relay-agent-1.0.0-darwin-arm64.tar.gz\n{sum} *relay-agent-1.0.0-darwin-arm64.tar.gz\n"
                ),
                "two archives for darwin-arm64",
            ),
        ] {
            std::fs::write(dir.path().join(SUMS), text).unwrap();
            let error = Release::open(dir.path()).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    #[test]
    fn an_archive_with_other_members_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        std::fs::write(stage.path().join(MEMBER), b"x").unwrap();
        std::fs::write(stage.path().join("extra"), b"y").unwrap();
        let file = "relay-agent-1.0.0-darwin-arm64.tar.gz";
        assert!(Command::new(TAR)
            .arg("-C")
            .arg(stage.path())
            .arg("-czf")
            .arg(dir.path().join(file))
            .args([MEMBER, "extra"])
            .status()
            .unwrap()
            .success());
        let bytes = std::fs::read(dir.path().join(file)).unwrap();
        std::fs::write(
            dir.path().join(SUMS),
            format!("{}  {file}\n", sha256_hex(&bytes)),
        )
        .unwrap();
        let error = Release::open(dir.path())
            .unwrap()
            .check_members("darwin-arm64")
            .unwrap_err();
        assert!(error.contains("must hold the one file"), "{error}");
    }
}
