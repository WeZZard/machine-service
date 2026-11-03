//! The `launchctl` that loads, unloads and inspects this service's own
//! LaunchAgents: the relay agent's (`docs/arch/relay-agent.md` RA-2) and the
//! installation's (`docs/arch/installation.md` IN-3); DC-241,
//! DC-242.
//!
//! A release always runs [`LAUNCHCTL`], macOS's only system `launchctl`,
//! and never one that `PATH` finds: whoever controls the service's
//! environment could otherwise answer for launchd, as DC-239 found for
//! `codesign`. Nothing in a release's environment changes that.
//!
//! A test build is the one exception. The feature [`TEST_FEATURE`], which
//! only the workspace's dev-dependencies enable, so only `cargo test`
//! builds it into a binary, makes [`Launchctl::resolve`] run the program
//! that [`TEST_VARIABLE`] names: a test's stub or its fake launchd. Without
//! one, or when it names the system `launchctl`, a test build refuses, so no
//! binary a test runs can load anything into the Mac's own desktop session.

use std::path::{Path, PathBuf};
use std::process::Command;

/// macOS's only system `launchctl`.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The Cargo feature that builds the test seam in.
pub const TEST_FEATURE: &str = "test-launchd";

/// The variable a test build reads for its test's own `launchctl`. A
/// release never reads it.
pub const TEST_VARIABLE: &str = "MACHINE_SERVICE_TEST_LAUNCHCTL";

/// Whether this build carries the test seam.
pub fn is_test_build() -> bool {
    cfg!(feature = "test-launchd")
}

/// The `launchctl` that one operation runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launchctl {
    program: PathBuf,
}

impl Launchctl {
    /// The system `launchctl`, [`LAUNCHCTL`].
    #[cfg(not(feature = "test-launchd"))]
    pub fn resolve() -> Result<Launchctl, String> {
        Ok(Launchctl {
            program: PathBuf::from(LAUNCHCTL),
        })
    }

    /// The test's own `launchctl`, which [`TEST_VARIABLE`] names; never the
    /// system one.
    #[cfg(feature = "test-launchd")]
    pub fn resolve() -> Result<Launchctl, String> {
        test_program(std::env::var_os(TEST_VARIABLE).map(PathBuf::from))
            .map(|program| Launchctl { program })
    }

    /// The program it runs.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// A command that runs it with `arguments`.
    pub fn command<I, S>(&self, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(&self.program);
        command.args(arguments);
        command
    }
}

/// Whether `program` is the system `launchctl`, by any path to it.
pub fn is_system(program: &Path) -> bool {
    let system = Path::new(LAUNCHCTL);
    program == system
        || match (program.canonicalize(), system.canonicalize()) {
            (Ok(program), Ok(system)) => program == system,
            _ => false,
        }
}

/// A test build's `launchctl`: the absolute path a test named, never the
/// system one.
#[cfg_attr(not(feature = "test-launchd"), allow(dead_code))]
fn test_program(named: Option<PathBuf>) -> Result<PathBuf, String> {
    match named {
        Some(program) if program.is_absolute() && !is_system(&program) => Ok(program),
        _ => Err(format!(
            "this is a test build ({TEST_FEATURE}), which runs only the launchctl a test names in {TEST_VARIABLE} and never {LAUNCHCTL}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_build_never_runs_the_system_launchctl() {
        for named in [
            None,
            Some(PathBuf::from("")),
            Some(PathBuf::from("launchctl")),
            Some(PathBuf::from(LAUNCHCTL)),
            Some(PathBuf::from("/bin/../bin/launchctl")),
        ] {
            let refused = test_program(named.clone()).unwrap_err();
            assert!(refused.contains(TEST_VARIABLE), "{named:?}: {refused}");
        }
        let stub = PathBuf::from("/tmp/a-test/launchctl");
        assert_eq!(test_program(Some(stub.clone())).unwrap(), stub);
    }

    /// No source of the service, its installer or the relay agent runs a
    /// `launchctl` that `PATH` finds.
    #[test]
    fn no_source_runs_launchctl_from_path() {
        fn sources(folder: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(folder).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    found.push(path);
                }
            }
        }
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut found = Vec::new();
        for name in [
            "machine-service-core",
            "machine-service",
            "machine-service-install",
            "relay-agent",
        ] {
            sources(&crates.join(name).join("src"), &mut found);
        }
        assert!(found.len() > 10, "{found:?}");
        let by_path = format!("Command::new({:?})", "launchctl");
        for path in found {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(!text.contains(&by_path), "{}", path.display());
        }
    }

    #[test]
    fn the_test_seam_follows_the_feature() {
        // Workspace tests build the seam in (DC-242); a release
        // build resolves the system launchctl whatever its environment.
        match (is_test_build(), Launchctl::resolve()) {
            (false, resolved) => assert_eq!(resolved.unwrap().program(), Path::new(LAUNCHCTL)),
            (true, Ok(launchctl)) => assert!(!is_system(launchctl.program())),
            (true, Err(refused)) => assert!(refused.contains(TEST_VARIABLE), "{refused}"),
        }
    }
}
