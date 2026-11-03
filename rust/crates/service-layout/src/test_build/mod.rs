//! Test builds: what a binary that `cargo test` built may touch, and how it
//! is told from a release (DC-251 to DC-258).
//!
//! The feature [`FEATURE`], which only the workspace's dev-dependencies
//! enable, builds the test seams into the daemon, the installer, the CLI and
//! the relay agent that the tests start, as DC-242 did for `launchctl`. A
//! test build
//!
//! - carries a marker in its executable, by which a release's installer
//!   refuses to install it ([`carries_marker`]);
//! - refuses the installed defaults: the ports [`INSTALLED_PORTS`], the
//!   folders [`INSTALLED_FOLDERS`] under the account's own home, and any
//!   bind outside loopback ([`owned_port`], [`owned_path`], [`owned_bind`]);
//! - reads its seams, such as its tool table, only from the `MACHINE_SERVICE_TEST_*`
//!   variables through [`seam`], which a release never reads;
//! - watches the lifeline its test hands it ([`start`]) and ends its whole
//!   process tree when the test is gone.
//!
//! A release has none of it: every function here answers as if no test
//! existed, without reading any variable.

use std::path::{Path, PathBuf};

#[cfg(feature = "test-build")]
pub mod harness;
mod processes;

pub use processes::{related, Row};

/// The Cargo feature that builds the test seams in.
pub const FEATURE: &str = "test-build";

/// The variable that hands a test child its lifeline.
pub const LIFELINE_VARIABLE: &str = "MACHINE_SERVICE_TEST_LIFELINE";

/// The ports of the installed service: its HTTP port and its relay session
/// port (BK-1).
pub const INSTALLED_PORTS: [u16; 2] = [6240, 6241];

/// The installed folders under the account's home: the state, the
/// configuration, the executables, the Tart store, the credential packs and
/// the LaunchAgents.
pub const INSTALLED_FOLDERS: [&str; 6] = [
    ".local/state/machine-service",
    ".config/machine-service",
    ".local/libexec/machine-service",
    ".tart",
    ".config/vm-credentials",
    "Library/LaunchAgents",
];

/// The variable by which the opt-in live layer (TL-6) starts a test build.
pub const LIVE_VARIABLE: &str = "MACHINE_SERVICE_TEST_LIVE";

/// The one value of [`LIVE_VARIABLE`] that starts the live layer.
pub const LIVE_VALUE: &str = "TL-6";

/// Whether this build carries the test seams.
pub const fn is_test_build() -> bool {
    cfg!(feature = "test-build")
}

/// Whether the opt-in live layer (TL-6) started this test build: it then
/// runs the system's own tools, reads the Mac's name, takes the host-wide
/// lease lock and sweeps with the whole process table, as a release does,
/// because what it proves is the real session. The installed ports and
/// folders, a bind outside loopback and an inherited `VM_ENVIRONMENT_FILE`
/// stay refused. The default suite never sets it: its children start from
/// an empty environment ([`harness`]). A release never reads it.
pub fn is_live() -> bool {
    #[cfg(feature = "test-build")]
    {
        std::env::var(LIVE_VARIABLE).is_ok_and(|value| value == LIVE_VALUE)
    }
    #[cfg(not(feature = "test-build"))]
    {
        false
    }
}

/// Whether this is a test build of the default suite, which acts only
/// through its seams: its tool table, its process fixture, its DNS-SD folder
/// and its name file.
pub fn is_isolated() -> bool {
    is_test_build() && !is_live()
}

// ----------------------------------------------------------------- marker

const KEY: u8 = 0x5a;

const fn xor<const N: usize>(bytes: &[u8; N]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut index = 0;
    while index < N {
        out[index] = bytes[index] ^ KEY;
        index += 1;
    }
    out
}

/// The marker, stored encoded so that only a test build's executable holds
/// its plain bytes; a release's installer, which searches for it, holds the
/// encoded form only and so never matches itself.
const ENCODED: [u8; 48] = xor(b"\0com.wezzard.machine-service:test-build:refuse\0\0");

#[cfg(feature = "test-build")]
#[used]
static MARKER: [u8; 48] = xor(&ENCODED);

/// The marker's plain bytes, decoded at run time.
fn marker() -> Vec<u8> {
    ENCODED
        .iter()
        .map(|byte| std::hint::black_box(*byte) ^ KEY)
        .collect()
}

/// Whether `bytes`, an executable, was built with the test seams.
pub fn carries_marker(bytes: &[u8]) -> bool {
    let marker = marker();
    let first = marker[1];
    bytes
        .windows(marker.len())
        .any(|window| window[1] == first && window == marker.as_slice())
}

/// Refuse the executable at `path` when it is a test build: an installation
/// never holds one (DC-258).
pub fn refuse_test_build(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    refuse_test_bytes(&path.display().to_string(), &bytes)
}

/// [`refuse_test_build`] of an executable already read, named `name`.
pub fn refuse_test_bytes(name: &str, bytes: &[u8]) -> Result<(), String> {
    if carries_marker(bytes) {
        Err(format!(
            "{name} was built by `cargo test` with the test seams ({FEATURE}) and must never be \
installed; build it with `cargo build --release`"
        ))
    } else {
        Ok(())
    }
}

// ------------------------------------------------------------------ start

/// Start a binary: in a test build, keep the marker in its executable and
/// watch the lifeline its test handed it. A release does nothing.
pub fn start() {
    #[cfg(feature = "test-build")]
    {
        std::hint::black_box(&MARKER);
        processes::watch_lifeline();
    }
}

// ------------------------------------------------------------------- seams

/// A test seam's path: in a test build, the absolute path the variable
/// `name` holds; in a release, `None`, without reading the variable.
pub fn seam(name: &str) -> Option<PathBuf> {
    #[cfg(feature = "test-build")]
    {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }
    #[cfg(not(feature = "test-build"))]
    {
        let _ = name;
        None
    }
}

/// The test's tool folder ([`crate::tools`]).
pub fn tools_folder() -> Option<PathBuf> {
    seam(crate::tools::TEST_VARIABLE)
}

/// The system tools a test may let run for real: each only reads, or
/// writes only where its caller points it.
pub const READ_ONLY_TOOLS: [crate::tools::Tool; 8] = {
    use crate::tools::Tool;
    [
        Tool::Codesign,
        Tool::Tar,
        Tool::Plutil,
        Tool::Sh,
        Tool::Zsh,
        Tool::SwVers,
        Tool::Ioreg,
        Tool::SshKeygen,
    ]
};

/// Link `tools`' system executables into the tool folder `folder`.
pub fn link_system_tools(folder: &Path, tools: &[crate::tools::Tool]) -> std::io::Result<()> {
    std::fs::create_dir_all(folder)?;
    for tool in tools {
        let Some(system) = tool.system() else {
            continue;
        };
        let link = folder.join(tool.name());
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(system, link)?;
    }
    Ok(())
}

/// In a test build, the processes this one may signal beyond its own
/// records: those it started, by descent or by its lifeline
/// ([`processes::descendants`]); a sweep never signals any other
/// (DC-253). A release answers `None`: no such limit.
pub fn signalable() -> Option<std::collections::HashSet<i32>> {
    is_test_build().then(processes::descendants)
}

// ------------------------------------------------------------------ guard

/// The account's own home, from the password database, whatever `HOME`
/// says.
pub fn account_home() -> Option<PathBuf> {
    // SAFETY: `getuid` only reads process state.
    let uid = unsafe { libc::getuid() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: `passwd` is plain old data; zeroed is a valid value.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer refers to a live, writable buffer of the size
    // passed; `result` is set to `entry` or null.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` points into `buffer`, NUL-terminated by libc.
    let home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    Some(PathBuf::from(
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(home.to_bytes()),
    ))
}

/// `path` with symbolic links resolved as far as it exists, and the rest
/// kept as written, so a link cannot hide an installed folder.
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The installed folder of `home` that holds `path`, if any.
pub fn installed_folder(home: &Path, path: &Path) -> Option<PathBuf> {
    let path = resolved(path);
    let home = resolved(home);
    INSTALLED_FOLDERS
        .iter()
        .map(|folder| home.join(folder))
        .find(|folder| path.starts_with(folder))
}

/// In a test build, refuse `port` when the installed service uses it.
pub fn owned_port(name: &str, port: u16) -> Result<(), String> {
    if is_test_build() && INSTALLED_PORTS.contains(&port) {
        return Err(format!(
            "this is a test build ({FEATURE}), which never uses the installed service's port {port} ({name}); a test names its own"
        ));
    }
    Ok(())
}

/// In a test build, refuse `path` when it lies in an installed folder of
/// the account.
pub fn owned_path(name: &str, path: &Path) -> Result<(), String> {
    if !is_test_build() {
        return Ok(());
    }
    match account_home().and_then(|home| installed_folder(&home, path)) {
        Some(folder) => Err(format!(
            "this is a test build ({FEATURE}), which never touches the installed {} ({name} is {}); a test names its own folder",
            folder.display(),
            path.display()
        )),
        None => Ok(()),
    }
}

/// In a test build, refuse to bind `host` unless it is loopback.
pub fn owned_bind(name: &str, host: &str) -> Result<(), String> {
    if is_test_build() && !is_loopback(host) {
        return Err(format!(
            "this is a test build ({FEATURE}), which binds only loopback; {name} is {host:?}"
        ));
    }
    Ok(())
}

/// Whether `host` names only this Mac's loopback interface.
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The host a listener the service opens on every interface binds in this
/// build: `0.0.0.0` in a release, loopback in a test build.
pub fn listener_host() -> &'static str {
    if is_test_build() {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_found_only_in_a_test_build() {
        let mut bytes = vec![7u8; 1000];
        assert!(!carries_marker(&bytes));
        bytes.splice(500..500, marker());
        assert!(carries_marker(&bytes));
        assert!(refuse_test_bytes("x", &bytes)
            .unwrap_err()
            .contains(FEATURE));
        // The encoded form, which every build holds, is not the marker.
        assert!(!carries_marker(&ENCODED));
        let own = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(carries_marker(&own), is_test_build());
    }

    #[test]
    fn installed_folders_are_found_through_links_and_dots() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        for folder in INSTALLED_FOLDERS {
            let inside = home.join(folder).join("x/y");
            assert_eq!(installed_folder(&home, &inside), Some(home.join(folder)));
        }
        std::fs::create_dir_all(home.join(".local/state")).unwrap();
        std::os::unix::fs::symlink(home.join(".local/state"), home.join("link")).unwrap();
        assert!(installed_folder(&home, &home.join("link/machine-service/state.json")).is_some());
        assert!(installed_folder(&home, &home.join(".local/state/other")).is_none());
        assert!(installed_folder(&home, &home.join("fixture/.tart")).is_none());
    }

    #[test]
    fn loopback_is_recognised() {
        for host in ["127.0.0.1", "localhost", "::1", "[::1]", "127.1.2.3"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["0.0.0.0", "", "::", "192.0.2.1", "example.com"] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn the_guard_follows_the_build() {
        assert_eq!(owned_port("p", 6240).is_err(), is_test_build());
        assert_eq!(owned_port("p", 6241).is_err(), is_test_build());
        assert!(owned_port("p", 50000).is_ok());
        assert_eq!(owned_bind("h", "0.0.0.0").is_err(), is_test_build());
        assert!(owned_bind("h", "127.0.0.1").is_ok());
        if let Some(home) = account_home() {
            let installed = home.join(".local/state/machine-service");
            assert_eq!(owned_path("s", &installed).is_err(), is_test_build());
        }
        assert!(owned_path("s", Path::new("/tmp/fixture/state")).is_ok());
    }
}
