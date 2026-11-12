//! The pinned GitHub Actions runner, installed into each runner VM at lease
//! time from a checksum-verified archive (LR-8, DC-272).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::labels::Kind;

/// The pinned runner version. Raise it, with both hashes, when GitHub asks.
pub const VERSION: &str = "2.338.2";

/// The pinned archives: platform and SHA-256, from the release's notes.
pub const ARCHIVES: [(&str, &str); 2] = [
    (
        "osx-arm64",
        "25a065af409ffa317ab4d5a3b614dd15fc9c40f2172bc1b35a1b1258e919a388",
    ),
    (
        "linux-arm64",
        "670adb2f268fe21dcbb58b3db0e0aa9d5b3bddfe2c651481336e561ff86de945",
    ),
];

/// Where the controller puts a job's files in the guest before the start
/// script moves them.
pub const GUEST_DIR: &str = "/tmp/lab-runner";

/// The runner platform of a guest kind (the VM hosts are Apple silicon).
pub fn platform(kind: Kind) -> &'static str {
    match kind {
        Kind::Macos => "osx-arm64",
        Kind::Linux => "linux-arm64",
    }
}

/// A platform's pinned SHA-256.
pub fn pin(platform: &str) -> Option<&'static str> {
    ARCHIVES
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, sha)| *sha)
}

/// The archive's file name.
pub fn archive_name(platform: &str) -> String {
    format!("actions-runner-{platform}-{VERSION}.tar.gz")
}

/// The archive's URL on GitHub's release.
pub fn archive_url(platform: &str) -> String {
    format!(
        "https://github.com/actions/runner/releases/download/v{VERSION}/{}",
        archive_name(platform)
    )
}

/// The SHA-256 of `bytes`, in hex.
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The runner archive for a platform: its bytes, checked against `sha`.
pub fn read_checked(path: &Path, sha: &str) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let got = sha256(&bytes);
    if got != sha {
        return Err(format!(
            "{} has SHA-256 {got}, not the pinned {sha}",
            path.display()
        ));
    }
    Ok(bytes)
}

/// The pinned archive for `platform` from the cache folder `cache`,
/// downloaded once and checked against its pin before every use.
pub fn cached(cache: &Path, platform: &str) -> Result<(Vec<u8>, String), String> {
    let sha = pin(platform).ok_or_else(|| format!("no pinned runner for {platform}"))?;
    let path = cache.join(archive_name(platform));
    if let Ok(bytes) = read_checked(&path, sha) {
        return Ok((bytes, sha.to_string()));
    }
    std::fs::create_dir_all(cache).map_err(|error| format!("{}: {error}", cache.display()))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(archive_url(platform))
        .call()
        .map_err(|error| format!("runner download: {error}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "runner download: HTTP {}",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .reader()
        .read_to_end(&mut bytes)
        .map_err(|error| format!("runner download: {error}"))?;
    let got = sha256(&bytes);
    if got != sha {
        return Err(format!(
            "the downloaded runner has SHA-256 {got}, not the pinned {sha}; nothing was kept"
        ));
    }
    let mut file = tempfile::NamedTempFile::new_in(cache).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.persist(&path)
        .map_err(|error| error.error.to_string())?;
    Ok((bytes, sha.to_string()))
}

/// The guest paths of a job's files.
pub fn guest_path(name: &str) -> PathBuf {
    Path::new(GUEST_DIR).join(name)
}

/// The `/etc/hosts` lines for the lab hosts' names: each address with its
/// names, each also with `.local`.
pub fn hosts_lines(entries: &[(String, Vec<String>)]) -> String {
    let mut text = String::from("# lab hosts (lab-runners, LR-3)\n");
    for (address, names) in entries {
        let mut all: Vec<String> = Vec::new();
        for name in names {
            let bare = name.trim_end_matches(".local").to_string();
            for candidate in [bare.clone(), format!("{bare}.local")] {
                if !candidate.is_empty() && !all.contains(&candidate) {
                    all.push(candidate);
                }
            }
        }
        if !all.is_empty() {
            text.push_str(&format!("{address} {}\n", all.join(" ")));
        }
    }
    text
}

/// The script that installs and starts the runner in the guest's desktop
/// session (LR-3, LR-8). It checks the archive against its pin again,
/// unpacks it, adds the lab hosts' names, hands the plan to every step
/// through the runner's `.env`, reads the JIT config into the runner's
/// environment and removes it, and then becomes the runner, which takes
/// one job and exits. The JIT config is in no argument and no output.
pub fn start_script(kind: Kind, sha: &str) -> String {
    let flush = match kind {
        Kind::Macos => {
            "sudo -n /usr/bin/dscacheutil -flushcache || true\nsudo -n /usr/bin/killall -HUP mDNSResponder || true\n"
        }
        Kind::Linux => "",
    };
    let digest = match kind {
        Kind::Macos => "/usr/bin/shasum -a 256",
        Kind::Linux => "sha256sum",
    };
    format!(
        r#"set -eu
dir={dir}
got=$({digest} "$dir/runner.tar.gz" | awk '{{print $1}}')
if [ "$got" != "{sha}" ]; then
    echo "lab-runner: the runner archive does not match its pin" >&2
    exit 70
fi
runner="$HOME/actions-runner"
rm -rf "$runner"
mkdir -p "$runner" "$HOME/.lab"
tar -xzf "$dir/runner.tar.gz" -C "$runner"
rm -f "$dir/runner.tar.gz"
sudo -n /bin/sh -c 'cat >> /etc/hosts' < "$dir/hosts"
{flush}mv "$dir/lab-plan.json" "$HOME/.lab/lab-plan.json"
printf 'LAB_PLAN=%s\n' "$HOME/.lab/lab-plan.json" > "$runner/.env"
chmod 600 "$dir/jit"
ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat "$dir/jit")
export ACTIONS_RUNNER_INPUT_JITCONFIG
rm -f "$dir/jit"
cd "$runner"
echo "lab-runner: starting runner {version}"
exec ./run.sh
"#,
        dir = GUEST_DIR,
        version = VERSION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_is_pinned() {
        for kind in [Kind::Macos, Kind::Linux] {
            let sha = pin(platform(kind)).unwrap();
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert_eq!(
            archive_url("osx-arm64"),
            "https://github.com/actions/runner/releases/download/v2.338.2/actions-runner-osx-arm64-2.338.2.tar.gz"
        );
    }

    #[test]
    fn a_cached_archive_is_used_only_while_it_matches_its_pin() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.tar.gz");
        std::fs::write(&path, b"runner").unwrap();
        assert!(read_checked(&path, &sha256(b"runner")).is_ok());
        let error = read_checked(&path, pin("osx-arm64").unwrap()).unwrap_err();
        assert!(error.contains("not the pinned"), "{error}");
    }

    #[test]
    fn the_start_script_checks_the_pin_and_keeps_the_config_out_of_sight() {
        let script = start_script(Kind::Macos, "abc");
        assert!(script.contains(r#"if [ "$got" != "abc" ]"#));
        assert!(script.contains("ACTIONS_RUNNER_INPUT_JITCONFIG=$(cat \"$dir/jit\")"));
        assert!(script.contains("rm -f \"$dir/jit\""));
        assert!(script.contains("exec ./run.sh"));
        assert!(!script.contains("--jitconfig"), "never an argument");
        assert!(script.contains("dscacheutil"));
        assert!(!start_script(Kind::Linux, "abc").contains("dscacheutil"));
        assert!(start_script(Kind::Linux, "abc").contains("sha256sum"));
    }

    #[test]
    fn the_lab_hosts_get_their_bare_and_local_names() {
        let text = hosts_lines(&[
            (
                "192.0.2.3".into(),
                vec!["mac-a".into(), "mac-a.local".into()],
            ),
            ("192.0.2.4".into(), vec![]),
        ]);
        assert_eq!(
            text,
            "# lab hosts (lab-runners, LR-3)\n192.0.2.3 mac-a mac-a.local\n"
        );
    }
}
