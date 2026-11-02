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
| `relay-agent` | with Tart | `-linux` |

- A guest agent runs inside the guest as machine code. The host-native build serves macOS guests, and Linux guests need the `-linux` build. A kind-qualified file beside the daemon, `<name>-<kind>`, outranks the host-native one.
- The daemon resolves every helper through this crate: the console worker, both guest agents, the relay agent it uploads into a clone ([RA-7](relay-agent.md#ra-7-bootstrap)), the forwarder and the firewall installer it names in its fixes. A helper is an entry of the layout or it cannot be resolved.
- The installer requires the same list, so it can neither miss an executable the daemon looks up nor require one it never uses. A Mac without Tart needs the first three only (MG-4 step 5, DC-65).
- The override variables `VM_GUEST_SESSION_AGENT`, `VM_GUEST_CONSOLE_AGENT`, `VM_CONSOLE_WORKER` and `MACHINE_SERVICE_DOT_FORWARDER` still win over the layout.

## IN-2: Install into a folder

- `rust/scripts/build-release.sh` builds every executable in the release profile and puts each under its installed name in `target/machine-service/`.
- `machine-service-install install --prefix DIR`, run from that folder, checks that every executable of the layout is beside it, then, after the old job has stopped, copies exactly those executables into `DIR`, each through a temporary file and a rename with mode `0755`. The LaunchAgent runs `DIR/machine-service`, and `~/.local/bin/machctl` links to `DIR/machctl`.
- A missing or non-executable file refuses the installation by name before anything is copied, written or loaded. Nothing else in the build folder is copied.
- Without `--prefix`, the folder beside the installer is the install folder, as before. `--check` with `--prefix` checks the build and writes nothing. `--prefix` with `remove` is refused.
- Every other rule of an installation still holds: it refuses while any lease record exists, and it keeps the installed environment.

## Tests

- AC-46 runs the built installer into an empty temporary folder and compares the result with `service_layout::required`: the same file names, each byte-equal to its build and mode `0755`, and nothing else. Removing any one executable from the build refuses the installation with nothing written. A Mac without Tart receives the core only.
- IN-1's own test resolves every helper path the daemon computes and checks that each names a file of the installation.
