# Installation (IN)

## Why

- The daemon finds its helpers beside its own executable, and nothing kept the folder it runs from complete. The executables were copied by a hand-kept list in the README, and the installer checked a second hand-kept list. Neither named `guest-session-agent`, so on the installed service every exec, push and pull over the session link failed with `guest session agent unavailable at …/guest-session-agent` (DC-79).
- An installation is correct only when it holds every executable the daemon looks up. Two lists can disagree; one list cannot.

## IN-1: One layout

- The `service-layout` crate is the one list of the executables an installation places beside the daemon, with when each is needed and which guest kinds need a build of their own.

| Executable | Needed | Guest builds |
| --- | --- | --- |
| `machine-service` | always | |
| `machctl` | always | |
| `machine-service-install` | always | |
| `console-worker` | with Tart | |
| `guest-console-agent` | with Tart | `-linux` |
| `guest-session-agent` | with Tart | `-linux` |
| `vm-service-dot-forwarder` | with Tart | |
| `vm-service-guest-firewall` | with Tart | |

- The relay agent is not an executable of this list. The installation holds its published release instead, the folder `relay-agent-release/`: the archives `relay-agent-<version>-<platform>.tar.gz` and `SHA256SUMS`, exactly as `rust/scripts/build-relay-agent.sh` and the release workflow make them (DC-140). It needs the `darwin-<arch>` archive with Tart or with durable exposure on, and the `linux-<arch>` archive with Tart, on the host's own architecture.
- A guest agent runs inside the guest as machine code. The host-native build serves macOS guests, and Linux guests need the `-linux` build. A kind-qualified file beside the daemon, `<name>-<kind>`, outranks the host-native one.
- The daemon resolves every helper through this crate: the console worker, both guest agents, the release whose relay agent it uploads into a clone ([RA-7](relay-agent.md#ra-7-bootstrap)), the forwarder and the firewall installer it names in its fixes. A helper is an entry of the layout or it cannot be resolved.
- The installer requires the same list, so it can neither miss an executable the daemon looks up nor require one it never uses. A Mac without Tart needs the first three only (MG-4 step 5, DC-65).
- The override variables `VM_GUEST_SESSION_AGENT`, `VM_GUEST_CONSOLE_AGENT`, `VM_CONSOLE_WORKER` and `MACHINE_SERVICE_DOT_FORWARDER` still win over the layout. `MACHINE_SERVICE_RELAY_AGENT_RELEASE` names another release folder of the same form; no variable names a relay agent executable.

## IN-2: Install into a folder

- `rust/scripts/build-release.sh` builds every executable in the release profile and puts each under its installed name in `target/machine-service/`, and makes the relay agent's release there with `build-relay-agent.sh` for the platforms the layout names.
- `machine-service-install install --prefix DIR`, run from that folder, checks that every executable of the layout is beside it, then, after the old job has stopped, copies exactly those executables into `DIR`, each through a temporary file and a rename with mode `0755`. The LaunchAgent runs `DIR/machine-service`, and `~/.local/bin/machctl` links to `DIR/machctl`.
- A missing or non-executable file refuses the installation by name before anything is copied, written or loaded. Nothing else in the build folder is copied.
- The release is checked before anything is written: every archive matches `SHA256SUMS`, each needed platform has an archive holding the one file `relay-agent`, and on an exposed Mac the macOS executable is signed with the identifier `com.wezzard.relay-agent` (DC-135). It is copied verbatim through a temporary folder that replaces the old one. The `relay-agent` and `relay-agent-linux` files of the earlier layout are removed.
- Without `--prefix`, the folder beside the installer is the install folder, as before. `--check` with `--prefix` checks the build and writes nothing. `--prefix` with `remove` is refused.
- Every other rule of an installation still holds: it refuses while any lease record exists, and it keeps the installed environment.

## IN-3: The durable Mac's relay agent

- With durable exposure on, `install` installs the release's macOS relay agent for the operator's account, after the daemon has stopped and before it starts again ([RA-2](relay-agent.md#ra-2-where-the-relay-agent-runs), M4):
  - the executable at `~/.local/libexec/relay-agent/relay-agent`, as in every guest;
  - the per-machine agent credential `<state>/relay/durable-agent.credential` and the broker's authority `<state>/relay/tls/`, made when missing;
  - `~/.config/relay-agent/config.json`, mode 600, with `broker_url` `wss://localhost:<session port>/relay`, the credential and the authority;
  - the LaunchAgent `~/Library/LaunchAgents/com.wezzard.relay-agent.plist` (`LimitLoadToSessionType` `Aqua`, `RunAtLoad`, `KeepAlive`, `HOME` set), bootstrapped into `gui/<uid>`; without a desktop session the installation is refused.
- `--rotate-relay-credential` makes a new credential; the restarted daemon accepts only it.
- With exposure off, `install` removes that relay agent, and `remove` removes it with the service.
- A current durable lease refuses every change to the relay agent, because a stopping relay agent ends the commands it runs (DC-118).
- `machine-service-install relay-agent [--label L]` installs the relay agent alone, for the instance that `MACHINE_SERVICE_STATE` and `MACHINE_SERVICE_SESSION_PORT` name, and `remove-relay-agent [--label L]` removes it; neither touches machine-service's own LaunchAgent. Every path derives from `HOME` and the label, so a second instance's test agent (TL-6) is the same installation under another home and label.

## Tests

- AC-46 runs the built installer into an empty temporary folder and compares the result with `service_layout::required`: the same file names, each byte-equal to its build and mode `0755`, and nothing else. Removing any one executable from the build refuses the installation with nothing written. A Mac without Tart receives the core only.
- IN-1's own test resolves every helper path the daemon computes and checks that each names a file of the installation.
- The release tests install the release verbatim, refuse a tampered archive, a missing platform, an unsigned macOS executable and a missing release with nothing written, and remove the earlier layout's builds.
- IN-3's tests install the relay agent with exposure on and check its LaunchAgent, its executable against the release, its configuration and credential; keep the credential on reinstall and replace it on rotation; remove it with exposure off and on `remove`; refuse it during a durable lease and without a desktop session; and install a second instance's agent under its own label without touching machine-service's LaunchAgent.
