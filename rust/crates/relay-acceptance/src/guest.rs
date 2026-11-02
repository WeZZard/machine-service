//! Shell snippets that differ between the guests' systems.

/// A guest's system, from the relay agent's `ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS.
    Macos,
    /// Linux (Ubuntu).
    Linux,
}

impl Os {
    /// The system an agent reports, `macos` or `linux`.
    pub fn from_agent(os: &str) -> Os {
        if os == "macos" {
            Os::Macos
        } else {
            Os::Linux
        }
    }

    /// Print the SHA-256 of standard input, hex.
    pub fn sha256_stdin(self) -> &'static str {
        match self {
            Os::Macos => "shasum -a 256 | cut -d' ' -f1",
            Os::Linux => "sha256sum | cut -d' ' -f1",
        }
    }

    /// Print the relay agent's process ID.
    pub fn agent_pid(self) -> &'static str {
        match self {
            Os::Macos => {
                "launchctl print gui/$(id -u)/com.wezzard.relay-agent | awk '$1 == \"pid\" && $2 == \"=\" {print $3; exit}'"
            }
            Os::Linux => "systemctl --user show -p MainPID --value relay-agent.service",
        }
    }

    /// List the sockets that process `$pid` listens on, TCP or UDP.
    pub fn listening_of_pid(self) -> &'static str {
        match self {
            Os::Macos => {
                "{ lsof -nP -a -p \"$pid\" -iTCP -sTCP:LISTEN; lsof -nP -a -p \"$pid\" -iUDP; } 2>/dev/null | grep -v '^COMMAND' || true"
            }
            Os::Linux => "ss -H -lntup 2>/dev/null | grep \"pid=$pid,\" || true",
        }
    }

    /// Turn sshd off for good, so it stays off after a reboot, and print
    /// `ssh-off` when port 22 no longer answers.
    pub fn sshd_off(self) -> &'static str {
        match self {
            Os::Macos => {
                "sudo -n launchctl disable system/com.openssh.sshd; \
sudo -n launchctl bootout system/com.openssh.sshd 2>/dev/null; \
for i in 1 2 3 4 5 6 7 8 9 10; do nc -z -G 2 127.0.0.1 22 2>/dev/null || { echo ssh-off; exit 0; }; sleep 1; done; exit 1"
            }
            Os::Linux => {
                "sudo -n systemctl disable --now ssh.socket ssh.service >/dev/null 2>&1; \
sudo -n systemctl stop ssh.socket ssh.service >/dev/null 2>&1; \
for i in 1 2 3 4 5 6 7 8 9 10; do ss -H -ltn 'sport = :22' | grep -q . || { echo ssh-off; exit 0; }; sleep 1; done; exit 1"
            }
        }
    }

    /// Print `ssh-off` when port 22 does not answer.
    pub fn sshd_is_off(self) -> &'static str {
        match self {
            Os::Macos => "nc -z -G 2 127.0.0.1 22 2>/dev/null && echo ssh-on || echo ssh-off",
            Os::Linux => "ss -H -ltn 'sport = :22' | grep -q . && echo ssh-on || echo ssh-off",
        }
    }

    /// The size of file `$f` in bytes, or nothing when it is absent.
    pub fn size_of_f(self) -> &'static str {
        match self {
            Os::Macos => "stat -f %z \"$f\" 2>/dev/null",
            Os::Linux => "stat -c %s \"$f\" 2>/dev/null",
        }
    }

    /// Write `$n` mebibytes of random bytes to `$f`.
    pub fn random_file(self) -> &'static str {
        match self {
            Os::Macos => "dd if=/dev/urandom of=\"$f\" bs=1m count=\"$n\" 2>/dev/null",
            Os::Linux => "dd if=/dev/urandom of=\"$f\" bs=1M count=\"$n\" 2>/dev/null",
        }
    }

    /// Detach a command line from the relay agent, so that it outlives the
    /// call that started it and the agent itself: `$cmd` runs in a session
    /// of its own (macOS), or in a transient user unit outside the agent's
    /// control group (Linux).
    pub fn detached(self, command: &str) -> String {
        let quoted = shell_quote(command);
        match self {
            Os::Macos => format!(
                "nohup perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' sh -c {quoted} >/dev/null 2>&1 </dev/null &"
            ),
            Os::Linux => format!(
                "systemd-run --user --collect --quiet --unit acc-detached-$$-$(date +%s%N) sh -c {quoted}"
            ),
        }
    }

    /// Restart the relay agent the way its service manager would after a
    /// crash: a stop signal to its process, which launchd or systemd starts
    /// again.
    pub fn kill_agent(self) -> String {
        format!("kill -TERM $({})", self.agent_pid())
    }

    /// Reboot the guest.
    pub fn reboot(self) -> &'static str {
        match self {
            Os::Macos => "sudo -n shutdown -r now",
            Os::Linux => "sudo -n systemctl reboot",
        }
    }

    /// Print the seconds since the epoch with microseconds.
    pub fn now(self) -> &'static str {
        "perl -MTime::HiRes=time -e 'printf \"%.6f\\n\", time'"
    }
}

/// Quote `text` for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(Os::from_agent("macos"), Os::Macos);
        assert_eq!(Os::from_agent("linux"), Os::Linux);
        assert!(Os::Linux
            .detached("echo 'x'")
            .contains("systemd-run --user"));
    }
}
