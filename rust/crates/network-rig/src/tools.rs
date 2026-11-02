//! The rig's pinned tools, fetched and verified into a cache outside the
//! repository.
//!
//! Each tool is pinned to one version and, per platform, to one URL and the
//! SHA-256 of what that URL serves. A fetch downloads with `curl` into a new
//! empty directory, checks the SHA-256 before anything is unpacked or run, and
//! renames the result into place, so the cache only ever holds verified
//! files. Nothing needs root.
//!
//! The cache is `NETWORK_RIG_CACHE` when set, else `machine-service/network-rig`
//! under `XDG_CACHE_HOME`, else under `~/Library/Caches` on macOS and
//! `~/.cache` elsewhere.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::RigError;

/// A tool of the rig.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `toxiproxy-server`.
    Toxiproxy,
    /// `mitmdump`, mitmproxy's non-interactive front end.
    Mitmdump,
}

/// How a pinned download becomes an executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packaging {
    /// The download is the executable.
    Executable,
    /// A gzipped tar; the executable is at this path inside it.
    TarGz(&'static str),
}

/// One pinned build of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// The tool.
    pub tool: Tool,
    /// Its version.
    pub version: &'static str,
    /// `<os>-<arch>` in Rust's names (`macos-aarch64`, `linux-x86_64`).
    pub platform: &'static str,
    /// Where it is downloaded from.
    pub url: &'static str,
    /// The SHA-256 of the download, lowercase hex.
    pub sha256: &'static str,
    /// How the download becomes an executable.
    pub packaging: Packaging,
}

/// The pinned Toxiproxy version.
pub const TOXIPROXY_VERSION: &str = "2.13.0";
/// The pinned mitmproxy version. `edge/edge.py` checks it too.
pub const MITMPROXY_VERSION: &str = "12.3.1";

/// Every pinned build. Toxiproxy's hashes are those of the release's
/// `checksums.txt`; mitmproxy publishes Sigstore bundles rather than hash
/// lists, so its hashes were recorded when the version was pinned.
pub const PINS: &[Pin] = &[
    Pin {
        tool: Tool::Toxiproxy,
        version: TOXIPROXY_VERSION,
        platform: "macos-aarch64",
        url: "https://github.com/Shopify/toxiproxy/releases/download/v2.13.0/toxiproxy-server-darwin-arm64",
        sha256: "21e5d0cbfc4af6e9967f617ee7a1645fa26181220fee3961630311a8de14705c",
        packaging: Packaging::Executable,
    },
    Pin {
        tool: Tool::Toxiproxy,
        version: TOXIPROXY_VERSION,
        platform: "linux-aarch64",
        url: "https://github.com/Shopify/toxiproxy/releases/download/v2.13.0/toxiproxy-server-linux-arm64",
        sha256: "9a7e2db359153772f26f9382adf5cd7305ee9d4cb2d4956922b5df38fe16c37e",
        packaging: Packaging::Executable,
    },
    Pin {
        tool: Tool::Toxiproxy,
        version: TOXIPROXY_VERSION,
        platform: "linux-x86_64",
        url: "https://github.com/Shopify/toxiproxy/releases/download/v2.13.0/toxiproxy-server-linux-amd64",
        sha256: "bf529e8bb9a05cd293f45a3f1d0aa66ff9469680dcdca7e0ebeae21f1b83de89",
        packaging: Packaging::Executable,
    },
    Pin {
        tool: Tool::Mitmdump,
        version: MITMPROXY_VERSION,
        platform: "macos-aarch64",
        url: "https://downloads.mitmproxy.org/12.3.1/mitmproxy-12.3.1-macos-arm64.tar.gz",
        sha256: "9b7f7a8e3fd1b1544089c937c579ef1f0f503f0ea598c25bda9eab077c735b93",
        packaging: Packaging::TarGz("mitmproxy.app/Contents/MacOS/mitmdump"),
    },
    Pin {
        tool: Tool::Mitmdump,
        version: MITMPROXY_VERSION,
        platform: "linux-aarch64",
        url: "https://downloads.mitmproxy.org/12.3.1/mitmproxy-12.3.1-linux-aarch64.tar.gz",
        sha256: "97a21d57b2288f7e293e63d3b11af34f394a8bbf760e6cf101ed1d57057a1f40",
        packaging: Packaging::TarGz("mitmdump"),
    },
    Pin {
        tool: Tool::Mitmdump,
        version: MITMPROXY_VERSION,
        platform: "linux-x86_64",
        url: "https://downloads.mitmproxy.org/12.3.1/mitmproxy-12.3.1-linux-x86_64.tar.gz",
        sha256: "f7b4237f5702f6cd21357fe6e4680a7e905f8f9b816b872230d018ea696c34d5",
        packaging: Packaging::TarGz("mitmdump"),
    },
];

impl Tool {
    /// The tool's name.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Toxiproxy => "toxiproxy-server",
            Tool::Mitmdump => "mitmdump",
        }
    }
}

/// This machine's platform in the pins' terms.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The pin of `tool` for `platform`.
pub fn pin(tool: Tool, platform: &str) -> Result<&'static Pin, RigError> {
    PINS.iter()
        .find(|pin| pin.tool == tool && pin.platform == platform)
        .ok_or_else(|| RigError::Unsupported {
            tool: tool.name().into(),
            platform: platform.into(),
        })
}

/// The cache directory, by the rule in the module documentation.
pub fn cache_dir() -> Result<PathBuf, RigError> {
    cache_dir_from(&|name| std::env::var_os(name).filter(|value| !value.is_empty()))
}

fn cache_dir_from(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, RigError> {
    if let Some(dir) = var("NETWORK_RIG_CACHE") {
        return Ok(PathBuf::from(dir));
    }
    let base = match var("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = var("HOME").ok_or_else(|| RigError::Tool {
                tool: "cache".into(),
                reason: "neither NETWORK_RIG_CACHE, XDG_CACHE_HOME nor HOME is set".into(),
            })?;
            let home = PathBuf::from(home);
            if cfg!(target_os = "macos") {
                home.join("Library/Caches")
            } else {
                home.join(".cache")
            }
        }
    };
    Ok(base.join("machine-service").join("network-rig"))
}

/// The verified executable of `tool` for this platform, fetched on first use.
pub fn ensure(tool: Tool) -> Result<PathBuf, RigError> {
    ensure_pin(pin(tool, &platform())?, &cache_dir()?)
}

/// The verified executable of `pin` in `cache`, fetched on first use.
pub fn ensure_pin(pin: &Pin, cache: &Path) -> Result<PathBuf, RigError> {
    let fail = |reason: String| RigError::Tool {
        tool: pin.tool.name().into(),
        reason,
    };
    fs::create_dir_all(cache).map_err(RigError::io(format!("create {}", cache.display())))?;
    let _lock = CacheLock::take(cache)?;
    let home = cache.join(format!(
        "{}-{}-{}",
        pin.tool.name(),
        pin.version,
        pin.platform
    ));
    let marker = home.join(".verified");
    let executable = home.join(match pin.packaging {
        Packaging::Executable => pin.tool.name(),
        Packaging::TarGz(member) => member,
    });
    if fs::read_to_string(&marker).ok().as_deref() == Some(pin.sha256) && executable.is_file() {
        return Ok(executable);
    }
    if home.exists() {
        fs::remove_dir_all(&home).map_err(RigError::io(format!("remove {}", home.display())))?;
    }

    let staging = cache.join(format!(
        ".staging-{}-{}",
        pin.tool.name(),
        std::process::id()
    ));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(RigError::io("clear the staging directory"))?;
    }
    fs::create_dir(&staging).map_err(RigError::io("create the staging directory"))?;
    let result = (|| {
        let download = staging.join("download");
        let status = Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "3",
            ])
            .arg("--output")
            .arg(&download)
            .arg(pin.url)
            .status()
            .map_err(|error| fail(format!("run curl: {error}")))?;
        if !status.success() {
            return Err(fail(format!("curl {} failed with {status}", pin.url)));
        }
        let digest = sha256_file(&download)?;
        if digest != pin.sha256 {
            return Err(fail(format!(
                "{} has SHA-256 {digest}, but {} is pinned",
                pin.url, pin.sha256
            )));
        }
        let tree = staging.join("tree");
        fs::create_dir(&tree).map_err(RigError::io("create the unpack directory"))?;
        match pin.packaging {
            Packaging::Executable => {
                fs::rename(&download, tree.join(pin.tool.name()))
                    .map_err(RigError::io("move the download"))?;
            }
            Packaging::TarGz(_) => {
                let status = Command::new("tar")
                    .arg("-xzf")
                    .arg(&download)
                    .arg("-C")
                    .arg(&tree)
                    .status()
                    .map_err(|error| fail(format!("run tar: {error}")))?;
                if !status.success() {
                    return Err(fail(format!("tar failed with {status}")));
                }
            }
        }
        let inside = tree.join(executable.strip_prefix(&home).expect("built under home"));
        let mut permissions = fs::metadata(&inside)
            .map_err(|_| fail(format!("{} is not in the download", inside.display())))?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        fs::set_permissions(&inside, permissions).map_err(RigError::io("make it executable"))?;
        fs::write(tree.join(".verified"), pin.sha256).map_err(RigError::io("write the marker"))?;
        fs::rename(&tree, &home).map_err(RigError::io(format!("install {}", home.display())))
    })();
    let _ = fs::remove_dir_all(&staging);
    result.map(|()| executable)
}

/// The SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, RigError> {
    let mut file = File::open(path).map_err(RigError::io(format!("open {}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(RigError::io("read the download"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// An exclusive `flock` on the cache, held while one fetch runs, so that
/// parallel tests and processes never fetch into the same place at once.
struct CacheLock {
    _file: File,
}

impl CacheLock {
    fn take(cache: &Path) -> Result<CacheLock, RigError> {
        let path = cache.join(".lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(RigError::io(format!("open {}", path.display())))?;
        // SAFETY: flock on a descriptor this function owns; no memory is shared.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(RigError::io("lock the cache")(
                std::io::Error::last_os_error(),
            ));
        }
        Ok(CacheLock { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn every_supported_platform_has_both_tools() {
        for platform in ["macos-aarch64", "linux-aarch64", "linux-x86_64"] {
            for tool in [Tool::Toxiproxy, Tool::Mitmdump] {
                let pin = pin(tool, platform).unwrap();
                assert_eq!(pin.sha256.len(), 64);
                assert!(pin
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
                assert!(pin.url.starts_with("https://"));
                assert!(pin.url.contains(pin.version));
            }
        }
        assert!(pin(Tool::Toxiproxy, "windows-x86_64").is_err());
    }

    #[test]
    fn the_cache_follows_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            cache_dir_from(&env(&[("NETWORK_RIG_CACHE", "/c"), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/c")
        );
        assert_eq!(
            cache_dir_from(&env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])).unwrap(),
            PathBuf::from("/x/machine-service/network-rig")
        );
        let home = cache_dir_from(&env(&[("HOME", "/h")])).unwrap();
        assert!(home.starts_with("/h") && home.ends_with("machine-service/network-rig"));
        assert!(cache_dir_from(&env(&[])).is_err());
    }

    #[test]
    fn a_cached_tool_is_used_without_a_download() {
        let cache = tempfile::tempdir().unwrap();
        let pin = Pin {
            tool: Tool::Toxiproxy,
            version: "0.0.0",
            platform: "test",
            url: "https://invalid.invalid/never-fetched",
            sha256: "00",
            packaging: Packaging::Executable,
        };
        let home = cache.path().join("toxiproxy-server-0.0.0-test");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("toxiproxy-server"), b"").unwrap();
        fs::write(home.join(".verified"), "00").unwrap();
        assert_eq!(
            ensure_pin(&pin, cache.path()).unwrap(),
            home.join("toxiproxy-server")
        );
    }

    #[test]
    fn a_download_with_the_wrong_hash_is_refused_and_not_kept() {
        let cache = tempfile::tempdir().unwrap();
        let source = cache.path().join("source");
        fs::write(&source, b"not the pinned bytes").unwrap();
        let url: &'static str = Box::leak(format!("file://{}", source.display()).into_boxed_str());
        let pin = Pin {
            tool: Tool::Toxiproxy,
            version: "0.0.0",
            platform: "test",
            url,
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            packaging: Packaging::Executable,
        };
        let error = ensure_pin(&pin, cache.path()).unwrap_err().to_string();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!cache.path().join("toxiproxy-server-0.0.0-test").exists());
    }

    #[test]
    fn a_download_with_the_pinned_hash_is_installed() {
        let cache = tempfile::tempdir().unwrap();
        let source = cache.path().join("source");
        fs::write(&source, b"#!/bin/sh\n").unwrap();
        let sha: &'static str = Box::leak(sha256_file(&source).unwrap().into_boxed_str());
        let url: &'static str = Box::leak(format!("file://{}", source.display()).into_boxed_str());
        let pin = Pin {
            tool: Tool::Mitmdump,
            version: "0.0.0",
            platform: "test",
            url,
            sha256: sha,
            packaging: Packaging::Executable,
        };
        let path = ensure_pin(&pin, cache.path()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o755, 0o755);
        assert_eq!(fs::read(&path).unwrap(), b"#!/bin/sh\n");
    }
}
