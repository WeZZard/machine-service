# Profile: Operator

The human operator (WeZZard) who installs the service, keeps it healthy, troubleshoots failures, and treats the host doctrines as non-negotiable.

## US11 — Drive the service from a CLI

**As an** operator, **I want** a `machctl` command for every HTTP operation, **so that** I can inspect and manage leases without hand-crafting HTTP.

- `acquire`, `wait --timeout-s`, `list [--json]`, `images [--json]`, `images-show`, `status`, `exec`, `push`, `pull`, `heartbeat`, `release`, `gc`.
- Flags mirror the API: `--image`, `--env`, `--ttl-hours`, `--no-wait`; old `--line`/`--pack` flags still parse (hidden aliases).
- Errors are actionable: `leased`/`limit`/`BASE-MISSING`/`404`/`unreachable` each map to a distinct, human-readable message.

**Tests:** `TestVmctl.test_acquire_old_flag_aliases` and the rest of `TestVmctl` (integration — real `vmctl` subprocesses against the live daemon: acquire with new and old flags, list/images/images-show, exec rc passthrough, gc, actionable error messages).

## US12 — Stable, backward-compatible API naming

**As an** operator, **I want** the `line`→`image` and `pack`/`lane`→`env` rename to be non-breaking, **so that** existing scripts and stored state keep working across the rename.

- `POST /acquire` accepts `image`/`env` and still accepts `line`/`pack`/`lane` as hidden aliases.
- Old state records (`line`/`line_kind`/`pack`/`lane`) migrate to the new shape on load, idempotently.
- User-facing errors use the new vocabulary (`unknown image 'x' (available: macos26, ubuntu2404)`).

**Tests:** `TestHttpApi.test_acquire_old_field_aliases_on_the_wire` (integration), `TestStateMigration.test_lane_alias_migrated` + `test_migration_idempotent` (unit).

## US13 — Inspect what the service is doing

**As an** operator, **I want** visibility into leases, capacity, and files, **so that** I can troubleshoot and verify the doctrines hold.

- `GET /vms`, `GET /vms/<name>`, `GET /images`, `GET /health`.
- State at `~/.local/state/machine-service/` (`state.json`, `state.lock`, `service.log`); per-VM console logs at `/tmp/tart-run-<vm>.log`.
- The install script manages the `com.wezzard.machine-service` LaunchAgent; restart with `launchctl kickstart -k gui/$UID/com.wezzard.machine-service`.

**Tests:** `TestImagesSnapshot` (unit capacity reporting), `TestSnapshots` (integration, live daemon over HTTP), `TestStateMigration` (unit, state file shape).

## US14 — A refused boot fails fast

**As an** operator, **I want** the daemon to detect a guest the hypervisor refused to start within seconds, **so that** a consumed host-wide slot costs seconds of diagnosis, not a 420-second `wait_ip` timeout followed by a vague rollback.

- Apple's 2-macOS-guest limit is host-wide across all Virtualization.framework consumers. If an external tool holds a slot, our `tart run` exits almost immediately — the daemon watches for that early exit and aborts the acquire at once, with an error naming the likely cause (`macOS VM refused to start — the host-wide 2-VM limit may be consumed by another Virtualization.framework user`).
- A guest that boots normally is indistinguishable from the old behavior: no false aborts, `wait_ip` continues as before.
- The failure is a clean rollback (US-N2): VM stopped, deleted, lease record dropped.
- When the `tart run` log says the hypervisor refused the guest because the number of VMs exceeds the system limit, the error is the capacity refusal, `macOS VM limit reached (the host refused to start <vm>: <the log line>)`, so a client that waits for capacity, such as mcp-relay, waits instead of failing (DC-18).

**Tests:** `TestRefusedBoot` (unit — Popen stub whose process exits immediately → fail-fast with diagnostic; alive process → no false failure).

## US15 — Capacity distinguishes our leases from host-wide guests

**As an** operator, **I want** the capacity report to separate vm-service's own macOS leases from host-wide Virtualization.framework guests, **so that** I can tell "our two slots are busy" apart from "an external tool took a slot" when the limit trips.

- `GET /images` capacity carries `host_macos_guests` (a pgrep gauge of running `com.apple.Virtualization.VirtualMachine` XPC processes — one per booted macOS guest, regardless of which tool started it) and `foreign_macos_guests` (host total minus our active macOS leases).
- The gauge is **advisory**: it appears in snapshot output and in the limit-tripped 409 message, but never gates acquires. A macOS image's `acquirable` uses it: the image is not acquirable when this service's active macOS leases, or the gauge less this service's other active guests when that is more, fill the limit. Capacity reports that figure as `macos_taken` (DC-18). Its snapshot semantics cannot see a foreign boot in flight, so enforcement stays on our own `ACTIVE_STATES` counter (US8).
- QEMU-based tools (e.g. UTM's QEMU backend) do not use Virtualization.framework, do not consume slots, and correctly do not appear in the gauge.

**Tests:** `TestHostGauge` (unit — stubbed process count drives gauge values in snapshot and 409 text).

## US16 — The host-wide limit is a documented shared resource

**As an** operator, **I want** the docs to state plainly that Apple's 2-macOS-guest limit is host-wide across all Virtualization.framework consumers while vm-service enforces only its own share, **so that** future changes (or other tools on this Mac) do not relearn this by a mysterious third-boot failure.

- This page and the README document: the shared-resource semantics, the advisory gauge (US15), the fail-fast diagnosis (US14), the QEMU exception, and that no SIP modification or framework configuration change is ever used to raise the limit (non-goals).

**Tests:** none — documentation contract, pinned by review like the non-goals section.

## US21 — Choose the guest link by measured results

- As the operator, I want both guest link candidates, SSH stdio and Tart exec over vsock, to pass one conformance suite on real VMs, so that I choose the default from evidence.
- The link must be a configuration choice, offline tests must need no Tart, SSH or network, and live tests must leave the installed service alone. See [test layers](../arch/test-layers.md).

**Tests:** AC-15 to AC-18 in [operator.acceptance](../acceptance/operator.acceptance).

## US-N4 — Tests run where they can, fast; e2e is explicitly real

**As an** operator, **I want** a test suite with no Tart, SSH, or network dependencies, **so that** I can validate changes in seconds before touching the live daemon.

- Stdlib `unittest` only; no pytest or other dependencies.
- Unit and integration suites stub `tart`, `_ssh`/`_scp`, and `subprocess.Popen` (via a proxy module so `subprocess.run` keeps working) and give every test an isolated temp state dir — no Tart, SSH, or network needed.
- Unit: 44 tests in ~0.1 s. Integration: 35 tests in ~6 s (real daemon process over real HTTP, `vmctl` subprocesses, Tart/SSH stubbed at process level).
- `tests/e2e/` is deliberately NOT stubbed: real Tart clone/boot, real sshpass SSH, real credential injection, run opt-in against the live daemon (`/opt/homebrew/bin/python3 -m unittest discover -s tests/e2e -v`). Every e2e acquire is released in teardown; the macOS-capacity test skips itself when slots are busy.

**Tests:** the unit and integration suites; suite-level properties (speed, isolation, stubbing) are part of the contract.

## US23 — See every machine on the intranet in one list

- As the operator, I want `machctl list` on any Mac to show every machine from every instance on the network, with nothing else to install or configure, so that I know what is leased, what is free and what each machine can do (DC-84).
- As the operator who centralises discovery in Consul, I want one switch that makes every instance and client use Consul instead (DC-84).
- An instance that does not answer stays in the list as unreachable, never silently missing. See [FD](../arch/federation.md) and [CL-1](../ux/cli/list.md#cl-1-the-machine-list).
- Each Mac appears once, under its own name, even when Consul knows it by another node name (DC-80).

**Tests:** AC-34, AC-37, AC-47, AC-48 and AC-49 in [operator.acceptance](../acceptance/operator.acceptance).

## US30 — Rename in one coordinated switch

- As the operator, I want the rename to machine-service and `machctl`, and every caller's switch to the new names, to land together, so that no old name lingers and no caller is left behind (DC-33).
- A caller that still uses an old path or variable fails at once with the new name in its error, instead of working unnoticed (DC-37). See [MG-3](../arch/rename.md#mg-3-one-coordinated-switch).
- The selected-environment profile is renamed with everything else: `vmctlPath` becomes `machctlPath` at `schemaVersion` 2, and an old profile is refused with its fix in the error (DC-41, DC-42).

**Tests:** AC-35, AC-36 and AC-45 in [operator.acceptance](../acceptance/operator.acceptance).

## US31 — Install on a Mac without Tart

- As the operator, I want to install machine-service on a Mac that has no Tart and no images and turn durable exposure on, so that the Mac offers its durable machine alone.
- See [MG-4](../arch/rename.md#mg-4-order-of-migration).

**Tests:** AC-38 in [operator.acceptance](../acceptance/operator.acceptance).

## US32 — The image list follows the images without a restart

- As the operator, I want adding, updating or removing an image line, promoting a base, or changing a base association to change the available virtual machines without restarting the daemon, so that image work and leasing do not wait on each other (DC-34).
- A broken image file never takes the list away: the last good list stays, and the failure is reported loudly until the file is fixed. See [IC](../arch/image-catalog.md).

**Tests:** AC-39 to AC-42 in [operator.acceptance](../acceptance/operator.acceptance).

## US33 — Choose which Macs are durable machines

- As the operator, I want each instance to expose its Mac as a durable machine only when I turn it on, so that a Mac that serves virtual machines, such as 192.0.2.3, is never leased as a whole by accident (DC-39).
- See [MM-1](../arch/machines.md#mm-1-two-kinds-of-machine).

**Tests:** AC-43 in [operator.acceptance](../acceptance/operator.acceptance).

## US34 — An installation holds every executable the service needs

- As the operator, I want one installation step to put every executable the daemon looks up beside it, and to refuse when the build lacks one, so that no operation fails later on a helper that was never installed (DC-79).
- See [IN](../arch/installation.md).

**Tests:** AC-46 in [operator.acceptance](../acceptance/operator.acceptance).

## US35 — Rename a Mac without breaking anything

- As the operator, I want to rename a Mac in System Settings at any time, so that its instance, its leases, `machctl` and every consumer keep working and simply show the new name (DC-86).
- See [MI](../arch/identity.md).

**Tests:** AC-50 to AC-54 in [operator.acceptance](../acceptance/operator.acceptance).

## US38 — Machines in other sites, behind NAT, serve leases

- As the operator, I want machines in other sites behind NAT to serve leases over a network that I choose, such as an intranet, Tailscale or Cloudflare Tunnel, without opening any inbound port on them (DC-100, DC-101).
- Any network that meets the [transport contract](../arch/transport-contract.md) works, and I check a new network once by reaching the broker through its advertised URL.

**Tests:** AC-66, AC-68 and AC-69 in [relay.acceptance](../acceptance/relay.acceptance).
