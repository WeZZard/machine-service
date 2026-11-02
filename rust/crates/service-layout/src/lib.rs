//! The executables a machine-service installation places beside the daemon
//! (`docs/arch/installation.md` IN-1, DC-79).
//!
//! The daemon finds its helpers next to its own executable: the console
//! worker it spawns, and the guest agents and the DNS-over-TLS forwarder it
//! uploads into a guest. The installer copies the same executables into the
//! install folder. Both read this one list, so an executable the daemon looks
//! up is an executable the installer requires and installs; a helper added to
//! the daemon without an entry here has no way to be resolved.
//!
//! This crate is a leaf: `console` must not depend on `machine-service-core`,
//! and both resolve through it.

use std::path::{Path, PathBuf};

/// When an installation needs an executable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// Every installation, including a Mac without Tart.
    Always,
    /// Only an instance that creates virtual machines, which is one that has
    /// Tart (MG-4 step 5, AC-38, DC-65).
    VirtualMachines,
}

/// One executable of the installation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Executable {
    /// The daemon the LaunchAgent runs.
    Service,
    /// The CLI, linked from `~/.local/bin/machctl`.
    Cli,
    /// The installer, kept beside the daemon for later installs, `remove`
    /// and `catalog-description`.
    Installer,
    /// The console worker the console manager spawns.
    ConsoleWorker,
    /// The guest console agent uploaded into a guest for a console.
    GuestConsoleAgent,
    /// The guest session agent uploaded into a guest for its guest session
    /// (GS-3): every exec, push and pull over the session link.
    GuestSessionAgent,
    /// The DNS-over-TLS forwarder uploaded into a macOS `internet` guest
    /// (DC-16).
    DotForwarder,
    /// The guest firewall installer the operator runs with `sudo`; the daemon
    /// names its path in the fix it reports.
    GuestFirewall,
    /// The relay agent uploaded into a guest whose image has none, or whose
    /// build differs (RA-6, RA-7).
    RelayAgent,
}

/// Every executable of the installation, in install order.
pub const ALL: [Executable; 9] = [
    Executable::Service,
    Executable::Cli,
    Executable::Installer,
    Executable::ConsoleWorker,
    Executable::GuestConsoleAgent,
    Executable::GuestSessionAgent,
    Executable::DotForwarder,
    Executable::GuestFirewall,
    Executable::RelayAgent,
];

impl Executable {
    /// The file name beside the daemon.
    pub const fn name(self) -> &'static str {
        match self {
            Executable::Service => "machine-service",
            Executable::Cli => "machctl",
            Executable::Installer => "machine-service-install",
            Executable::ConsoleWorker => "console-worker",
            Executable::GuestConsoleAgent => "guest-console-agent",
            Executable::GuestSessionAgent => "guest-session-agent",
            Executable::DotForwarder => "vm-service-dot-forwarder",
            Executable::GuestFirewall => "vm-service-guest-firewall",
            Executable::RelayAgent => "relay-agent",
        }
    }

    /// When an installation needs it.
    pub const fn need(self) -> Need {
        match self {
            Executable::Service | Executable::Cli | Executable::Installer => Need::Always,
            Executable::ConsoleWorker
            | Executable::GuestConsoleAgent
            | Executable::GuestSessionAgent
            | Executable::DotForwarder
            | Executable::GuestFirewall
            | Executable::RelayAgent => Need::VirtualMachines,
        }
    }

    /// The guest kinds that need a build of their own, installed as
    /// `<name>-<kind>`.
    ///
    /// A guest agent runs inside the guest as machine code, so its build must
    /// match the guest's operating system. The host-native build serves macOS
    /// guests, because the service runs on macOS; a Linux guest cannot execute
    /// it, so the Linux build is required (the probe otherwise fails with
    /// `Exec format error`). The forwarder serves macOS guests only.
    pub const fn guest_builds(self) -> &'static [&'static str] {
        match self {
            Executable::GuestConsoleAgent
            | Executable::GuestSessionAgent
            | Executable::RelayAgent => &["linux"],
            Executable::Service
            | Executable::Cli
            | Executable::Installer
            | Executable::ConsoleWorker
            | Executable::DotForwarder
            | Executable::GuestFirewall => &[],
        }
    }
}

/// The file name of `executable`'s build for guests of `kind`.
pub fn guest_build_name(executable: Executable, kind: &str) -> String {
    format!("{}-{kind}", executable.name())
}

/// The file names an installation requires, in install order.
///
/// `creates_virtual_machines` is false on a Mac without Tart, whose instance
/// offers only its durable machine.
pub fn required(creates_virtual_machines: bool) -> Vec<String> {
    let mut names = Vec::new();
    for executable in ALL {
        if executable.need() == Need::VirtualMachines && !creates_virtual_machines {
            continue;
        }
        names.push(executable.name().to_string());
        for kind in executable.guest_builds() {
            names.push(guest_build_name(executable, kind));
        }
    }
    names
}

/// The folder that holds the running executable, which is the install folder
/// when the daemon runs.
pub fn daemon_directory() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
}

/// The path of `executable` beside the running executable.
pub fn beside_daemon(executable: Executable) -> Option<PathBuf> {
    daemon_directory().map(|directory| directory.join(executable.name()))
}

/// The build of `executable` for a guest of `kind` beside the running
/// executable; see [`guest_build_in`].
pub fn guest_build_beside_daemon(executable: Executable, kind: &str) -> Option<PathBuf> {
    daemon_directory().map(|directory| guest_build_in(&directory, executable, kind))
}

/// The build of `executable` for a guest of `kind` in `directory`: the
/// kind-qualified `<name>-<kind>` when that file exists, otherwise the
/// host-native `<name>`, which serves guests whose OS matches the host.
pub fn guest_build_in(directory: &Path, executable: Executable, kind: &str) -> PathBuf {
    let qualified = directory.join(guest_build_name(executable, kind));
    if qualified.is_file() {
        return qualified;
    }
    directory.join(executable.name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn names_are_distinct() {
        let names: BTreeSet<String> = required(true).into_iter().collect();
        assert_eq!(names.len(), required(true).len());
    }

    #[test]
    fn a_vm_instance_requires_both_guest_agents_for_linux_guests() {
        let names = required(true);
        for name in [
            "guest-console-agent",
            "guest-console-agent-linux",
            "guest-session-agent",
            "guest-session-agent-linux",
            "console-worker",
            "vm-service-dot-forwarder",
            "vm-service-guest-firewall",
            "relay-agent",
            "relay-agent-linux",
        ] {
            assert!(names.iter().any(|candidate| candidate == name), "{name}");
        }
    }

    #[test]
    fn a_mac_without_tart_requires_the_core_only() {
        assert_eq!(
            required(false),
            ["machine-service", "machctl", "machine-service-install"]
        );
    }

    #[test]
    fn the_kind_qualified_build_outranks_the_host_native_one() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Executable::GuestSessionAgent;
        std::fs::write(directory.path().join("guest-session-agent"), b"host").unwrap();
        assert_eq!(
            guest_build_in(directory.path(), agent, "linux"),
            directory.path().join("guest-session-agent")
        );
        std::fs::write(directory.path().join("guest-session-agent-linux"), b"elf").unwrap();
        assert_eq!(
            guest_build_in(directory.path(), agent, "linux"),
            directory.path().join("guest-session-agent-linux")
        );
        assert_eq!(
            guest_build_in(directory.path(), agent, "macos"),
            directory.path().join("guest-session-agent")
        );
    }
}
