//! The external tools the service, its installer and its CLI run, and where
//! each one is found (DC-1xx-ISO-4).
//!
//! Every external program is a [`Tool`] of one table, and is run by the
//! absolute path the table gives. A release finds macOS's own tools at their
//! system paths, the same way DC-239 and DC-242 made `codesign` and
//! `launchctl` independent of `PATH`; a tool the operator installs, such as
//! Tart, has no system path, and its caller locates it as it
//! always has ([`located`]).
//!
//! A test build is different in one way only: its whole table is the folder
//! that [`TEST_VARIABLE`] names, which a test fills with its fakes and with
//! links to the read-only system tools it lets run. A tool missing from that
//! folder is refused, a bare name is never looked up on `PATH`, and an
//! absolute path to a system tool is mapped to the test's entry, so no test
//! can run an un-tabled tool by accident. A release never reads the variable.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The variable a test build reads for its tool folder. A release never
/// reads it.
pub const TEST_VARIABLE: &str = "MACHINE_SERVICE_TEST_TOOLS";

/// One external tool.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    /// Tart, which the operator installs.
    Tart,
    /// OpenSSH's client.
    Ssh,
    /// OpenSSH's copy.
    Scp,
    /// OpenSSH's key generator.
    SshKeygen,
    /// The POSIX shell.
    Sh,
    /// The login shell of durable leases.
    Zsh,
    /// `pgrep`, for the host's guest count.
    Pgrep,
    /// AppleScript, which asks an application to quit.
    Osascript,
    /// The code-signing tool (DC-239).
    Codesign,
    /// The archiver of relay agent releases.
    Tar,
    /// The property-list tool.
    Plutil,
    /// Launch Services' list of running applications.
    Lsappinfo,
    /// The user's cron table.
    Crontab,
    /// The I/O registry.
    Ioreg,
    /// The macOS version.
    SwVers,
    /// The system configuration, which names the Mac.
    Scutil,
}

/// Every tool.
pub const ALL: [Tool; 16] = [
    Tool::Tart,
    Tool::Ssh,
    Tool::Scp,
    Tool::SshKeygen,
    Tool::Sh,
    Tool::Zsh,
    Tool::Pgrep,
    Tool::Osascript,
    Tool::Codesign,
    Tool::Tar,
    Tool::Plutil,
    Tool::Lsappinfo,
    Tool::Crontab,
    Tool::Ioreg,
    Tool::SwVers,
    Tool::Scutil,
];

impl Tool {
    /// The tool's file name.
    pub const fn name(self) -> &'static str {
        match self {
            Tool::Tart => "tart",
            Tool::Ssh => "ssh",
            Tool::Scp => "scp",
            Tool::SshKeygen => "ssh-keygen",
            Tool::Sh => "sh",
            Tool::Zsh => "zsh",
            Tool::Pgrep => "pgrep",
            Tool::Osascript => "osascript",
            Tool::Codesign => "codesign",
            Tool::Tar => "tar",
            Tool::Plutil => "plutil",
            Tool::Lsappinfo => "lsappinfo",
            Tool::Crontab => "crontab",
            Tool::Ioreg => "ioreg",
            Tool::SwVers => "sw_vers",
            Tool::Scutil => "scutil",
        }
    }

    /// macOS's own path of the tool, or `None` for one the operator
    /// installs.
    pub const fn system(self) -> Option<&'static str> {
        match self {
            Tool::Tart => None,
            Tool::Ssh => Some("/usr/bin/ssh"),
            Tool::Scp => Some("/usr/bin/scp"),
            Tool::SshKeygen => Some("/usr/bin/ssh-keygen"),
            Tool::Sh => Some("/bin/sh"),
            Tool::Zsh => Some("/bin/zsh"),
            Tool::Pgrep => Some("/usr/bin/pgrep"),
            Tool::Osascript => Some("/usr/bin/osascript"),
            Tool::Codesign => Some("/usr/bin/codesign"),
            Tool::Tar => Some("/usr/bin/tar"),
            Tool::Plutil => Some("/usr/bin/plutil"),
            Tool::Lsappinfo => Some("/usr/bin/lsappinfo"),
            Tool::Crontab => Some("/usr/bin/crontab"),
            Tool::Ioreg => Some("/usr/sbin/ioreg"),
            Tool::SwVers => Some("/usr/bin/sw_vers"),
            Tool::Scutil => Some("/usr/sbin/scutil"),
        }
    }

    /// The tool with this file name.
    pub fn from_name(name: &str) -> Option<Tool> {
        ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// The tool whose system path this is.
    pub fn from_system(path: &Path) -> Option<Tool> {
        ALL.into_iter().find(|tool| {
            tool.system()
                .is_some_and(|system| Path::new(system) == path)
        })
    }
}

/// The path to run `tool` by: its system path in a release, the test's
/// entry in a test build. A tool without a system path is located by its
/// caller through [`located`].
pub fn path(tool: Tool) -> Result<PathBuf, String> {
    located(tool, || {
        tool.system()
            .map(PathBuf::from)
            .ok_or_else(|| format!("{} has no system path; its caller locates it", tool.name()))
    })
}

/// The path to run `tool` by: `release()`'s answer in a release, the test's
/// entry in a test build, whatever `release` would have found.
pub fn located(
    tool: Tool,
    release: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<PathBuf, String> {
    if crate::test_build::is_isolated() {
        let _ = release;
        test_entry(tool, crate::test_build::tools_folder().as_deref())
    } else {
        release()
    }
}

/// A path its caller found for `tool`, such as the Tart an installation
/// names: in a release, as found; in a test build, the test table's own
/// entry or a test's own file outside the system folders, as [`program`]
/// admits one, never an installed tool, such as a Homebrew Tart.
pub fn admitted(tool: Tool, found: &Path) -> Result<PathBuf, String> {
    if !crate::test_build::is_isolated() {
        return Ok(found.to_path_buf());
    }
    let same = path(tool).is_ok_and(|entry| {
        entry == found
            || matches!((found.canonicalize(), entry.canonicalize()), (Ok(a), Ok(b)) if a == b)
    });
    if same
        || (found.is_absolute() && !is_system_folder(found) && !is_system_folder(&resolved(found)))
    {
        Ok(found.to_path_buf())
    } else {
        Err(format!(
            "this is a test build, which runs {} only from its tool table ({TEST_VARIABLE}), not {}",
            tool.name(),
            found.display()
        ))
    }
}

/// A command that runs `tool` from the table.
pub fn command(tool: Tool) -> Result<Command, String> {
    path(tool).map(Command::new)
}

/// The program to run for an argument vector's first element: a bare tool
/// name or a system tool's path is the table's entry; any other absolute
/// path is run as it is. A release runs an unknown bare name as it always
/// did; a test build refuses it, and refuses a path into the system folders
/// that is no tool of the table.
pub fn program(argv0: &str) -> Result<PathBuf, String> {
    let given = Path::new(argv0);
    let tool = if given.is_absolute() {
        Tool::from_system(given)
    } else {
        Tool::from_name(argv0)
    };
    match tool {
        Some(tool) => path(tool),
        None if !crate::test_build::is_isolated() => Ok(given.to_path_buf()),
        None if given.is_absolute()
            && !is_system_folder(given)
            && !is_system_folder(&resolved(given)) =>
        {
            Ok(given.to_path_buf())
        }
        None => Err(format!(
            "this is a test build, which runs no tool outside its table ({TEST_VARIABLE}): {argv0}"
        )),
    }
}

/// A command for an argument vector's first element, through [`program`].
pub fn command_for(argv0: &str) -> Result<Command, String> {
    program(argv0).map(Command::new)
}

/// `path` with its links resolved, when it exists.
fn resolved(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` lies in a folder of macOS's or a package manager's own
/// tools.
pub fn is_system_folder(path: &Path) -> bool {
    [
        "/bin",
        "/sbin",
        "/usr",
        "/System",
        "/Library",
        "/opt",
        "/Applications",
    ]
    .iter()
    .any(|folder| path.starts_with(folder))
}

/// A test build's entry for `tool` in `folder`, the test's whole table.
/// Without one, the table is the read-only system tools
/// (`test_build::READ_ONLY_TOOLS`) at their system paths, so a library's
/// own tests can read a signature or an archive, and nothing else.
#[cfg_attr(not(feature = "test-build"), allow(dead_code))]
fn test_entry(tool: Tool, folder: Option<&Path>) -> Result<PathBuf, String> {
    let Some(folder) = folder.filter(|folder| folder.is_absolute()) else {
        return match tool.system() {
            Some(system) if crate::test_build::READ_ONLY_TOOLS.contains(&tool) => {
                Ok(PathBuf::from(system))
            }
            _ => Err(format!(
                "this is a test build, which runs {} only from the tool folder a test names in {TEST_VARIABLE}",
                tool.name()
            )),
        };
    };
    let entry = folder.join(tool.name());
    if is_executable(&entry) {
        Ok(entry)
    } else {
        Err(format!(
            "this is a test build, and its tool folder {} has no {}",
            folder.display(),
            tool.name()
        ))
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_system_paths_are_distinct_and_absolute() {
        let mut names = std::collections::HashSet::new();
        for tool in ALL {
            assert!(names.insert(tool.name()), "{tool:?}");
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
            if let Some(system) = tool.system() {
                assert!(Path::new(system).is_absolute());
                assert!(system.ends_with(tool.name()), "{system}");
                assert_eq!(Tool::from_system(Path::new(system)), Some(tool));
            }
        }
    }

    #[test]
    fn a_test_table_answers_only_its_own_entries() {
        let folder = tempfile::tempdir().unwrap();
        let refused = test_entry(Tool::Ssh, None).unwrap_err();
        assert!(refused.contains(TEST_VARIABLE), "{refused}");
        // Without a table, only the read-only system tools.
        assert_eq!(
            test_entry(Tool::Codesign, None).unwrap(),
            PathBuf::from("/usr/bin/codesign")
        );
        for tool in [
            Tool::Tart,
            Tool::Pgrep,
            Tool::Osascript,
            Tool::Scutil,
            Tool::Crontab,
        ] {
            assert!(test_entry(tool, None).is_err(), "{tool:?}");
        }
        let refused = test_entry(Tool::Ssh, Some(Path::new("relative"))).unwrap_err();
        assert!(refused.contains(TEST_VARIABLE), "{refused}");
        let refused = test_entry(Tool::Ssh, Some(folder.path())).unwrap_err();
        assert!(refused.contains("has no ssh"), "{refused}");
        let ssh = folder.path().join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\n").unwrap();
        assert!(test_entry(Tool::Ssh, Some(folder.path())).is_err());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(test_entry(Tool::Ssh, Some(folder.path())).unwrap(), ssh);
    }

    #[test]
    fn the_table_follows_the_build() {
        if crate::test_build::is_test_build() {
            // A test build maps a bare name and a system path to the test's
            // table, and refuses an unknown name or system path.
            for argv0 in ["no-such-tool", "/usr/bin/true"] {
                assert!(program(argv0).is_err(), "{argv0}");
            }
            assert_eq!(
                program("/tmp/a-test/helper").unwrap(),
                PathBuf::from("/tmp/a-test/helper")
            );
        } else {
            assert_eq!(path(Tool::Ssh).unwrap(), PathBuf::from("/usr/bin/ssh"));
            assert_eq!(program("ssh").unwrap(), PathBuf::from("/usr/bin/ssh"));
            assert_eq!(
                program("/usr/bin/true").unwrap(),
                PathBuf::from("/usr/bin/true")
            );
            assert!(path(Tool::Tart).is_err());
        }
    }
}
