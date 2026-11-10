# Profile: Agent

An AI agent or script that needs a disposable VM for a bounded task. Concrete embodiments: a pi child agent driven through `machctl`, a Claude Code session, a shell script doing build/test work.

The agent's contract with the service is **lease-based**: it asks for a VM, works inside it, transfers evidence out, and gives the VM back. It never touches `tart` and never holds host credentials beyond what the service injects into the guest.

## US1 — Acquire a fresh VM on demand

**As an** agent, **I want** to request a VM by `purpose` (a short slug that becomes the VM name), **so that** I get a machine dedicated to my task.

- `POST /acquire` or `machctl acquire --purpose <slug>`.
- Choose `image` (`macos26` or `ubuntu2404`), `env` pack, `ttl_hours`, optional `cpu`/`memory_mb`/`disk_gb` overrides.
- Acquisition is synchronous in both readiness modes. The default verifies key-only command access and a checksum-verified transfer round trip. `wait: false` uses a shorter bootstrap deadline and one command probe, but does not skip those readiness requirements.
- Cloning a macOS base can take minutes. The caller must allow a sufficiently long acquisition timeout.
- VM name: `pilot-mac-<purpose>-<rand>` (macOS) or `pilot-<purpose>-<rand>`.
- Purpose must match `[a-z0-9][a-z0-9-]{0,63}`; one active lease per purpose+image.

**Tests:** `TestAcquireValidation` (unit, validation), `TestAcquireLifecycle` (unit, happy path + exact tart call sequence), `TestLifecycleE2E` (e2e, real clone/boot/exec/release).

## US2 — Work inside the leased VM

**As an** agent, **I want** to run commands and move files, **so that** the VM does useful work and its outputs reach the host.

- `POST /vms/<vm>/exec` accepts `argv` or `script`. The script shell follows the image kind: zsh on macOS and bash on Ubuntu. The positive request timeout defaults to 600 seconds and is not clamped to 3600 seconds.
- `push` host→guest, `pull` guest→host (evidence transfer *before* release).
- Operations require the VM to be in `running` state.

**Tests:** `TestRecordOps` (unit), `TestLifecycleE2E.test_push_pull_roundtrip` (e2e, real file transfer).

## US3 — Credentials arrive automatically

**As an** agent, **I want** gateway credentials present as environment variables when my session starts, **so that** `pi` and Claude Code work without any interactive auth inside the guest.

- Default `env: "default"` injects `~/.config/vm-credentials/default/`: `env.extra` (LiteLLM/Anthropic gateway envs, `GH_TOKEN`) plus optional `git-identity`.
- The keys reach every zsh, interactive or not: the injection adds a line to `~/.zshenv` that loads `~/.config/zsh/secrets.zsh`, so a command run over `exec` sees them too (DC-96).
- A pack may also hold `home.tgz`, unpacked into the guest's home folder. It carries what an agent keeps in files rather than in environment variables: a sign-in token, or settings that name the gateway (DC-95).
- Delivery is verified: scp return codes are checked and the guest `secrets.zsh` must be non-empty, or the injection reports failure.
- The gateway architecture is transparent to the agent — it just sees working API endpoints.

**Tests:** `TestInjectPack` (unit; one test per failure mode: scp rc, script rc, verification probe, SSH drop), `TestGitIdentity` (unit), `TestCredentialInjectionE2E` (e2e; gateway envs reach the login shell, git identity applied, `env: "none"` stays credential-free).

## US4 — Opt out of credentials

**As an** agent, **I want** `env: "none"` for credential-free clones, **so that** I can run tasks that need no network identity (and prove images stay credential-free).

**Tests:** `TestAcquireValidation.test_env_none_records_no_pack`, `TestInjectPack.test_empty_pack_injects_nothing_and_succeeds` (unit), `TestCredentialInjectionE2E.test_none_pack_is_credential_free` (e2e).

## US5 — Pristine clone every time

**As an** agent, **I want** every acquire to be a fresh COW clone of the golden base, **so that** no state leaks between tasks.

- Only purpose clones are ever booted; the golden bases (`pilot-*-base`) are never started through the service.

**Tests:** `TestAcquireLifecycle.test_full_lifecycle_record_and_tart_calls`, `test_macos_base_never_booted_even_for_macos_line` (unit), `TestDoctrinesE2E.test_macos_base_never_booted` (e2e).

## US6 — Keep long jobs alive

**As an** agent, **I want** to heartbeat my lease, **so that** long tasks are not reclaimed mid-flight.

- `POST /machines/<vm>/heartbeat` with `ttl_hours` (0.1–720) resets the TTL and clears any grace/warn state.
- `POST /machines/<vm>/heartbeat` without `ttl_hours` resets the TTL to the lease's own initial `ttl_hours`, so `machctl heartbeat <vm>` keeps a lease alive.

**Tests:** `TestTtlGc` (unit: heartbeat resets, bounds, unknown VM), `test_bare_heartbeat_renews_expired_ttl` (unit).

## US7 — See capacity before asking

**As an** agent, **I want** `GET /images` (or `machctl images --json`), **so that** I can pick an image that actually has capacity.

**Tests:** `TestImagesSnapshot` (unit), `TestSnapshots` (integration), `TestServiceUp.test_daemon_and_images` (e2e).

## US17 — Discover acquisition options and inspect applied settings

- As an agent, I want to inspect supported acquisition options without creating a VM and distinguish requested settings from provisioning instructions and observed readiness.
- The former reporting implementation was removed. The new source implements [the acquisition contract](../acquisition-contract.md) without a Tart patch; `test_stock_console.py` and `test_console_http.py` verify its defaults, validation, and read-only discovery.
- Future reporting must preserve ordinary resource defaults, nullable legacy fields, selected environments, and existing leases without invented configuration.

## US18 — Let a human watch the same VM

- As a user, I want a normal vm-service deployment and mcp-vm-relay upgrade to let me watch the agent's existing guest desktop through a standard viewer while Tart remains unmodified.
- Closing or cancelling the viewer must leave the VM and agent running. Managed access must expire independently of long guest operations and delayed VM destruction.
- The previous patched-Tart implementation was rejected and removed. The new source implements [the revised guest-sharing design](../design.md), with platform-specific preparation and no hidden runtime patch.
- `test_stock_console.py`, `test_console_worker.py`, `test_guest_console.py`, and `test_console_http.py` cover the controller, real supervised helper with synthetic processes, guest adapters, and public contract. These are not real VM/viewer acceptance.
- [The console contract](../console-contract.md) defines the source interface. [The acceptance specification](../vnc-acceptance.md) still requires the installed relay path, actual viewer screenshots, live lifecycle behavior, and cleanup. macOS user authentication and display selection remain explicit unverified steps until observed.

## US19 — Many operations without many SSH handshakes

- As an agent, I want each exec, push and pull on my lease to cost one round trip on an open connection, so that an action made of many small operations is not dominated by connection setup.
- Results must be the same as today, transferred bytes must arrive intact or fail loudly, and an exec whose connection was lost must be reported uncertain, never run again. See [guest session](../arch/guest-session.md).

**Tests:** AC-1 to AC-5 in [agent.acceptance](../acceptance/agent.acceptance).

## US20 — Keep working across an unreliable network

- As an agent on another network or continent, I want my connection to vm-service to survive resets, silent drops and address changes, so that a long task does not fail because a network path broke.
- No operation may run twice because of a reconnect. After any failure I can learn whether an operation completed. See [remote session](../arch/remote-session.md).

**Tests:** AC-6 to AC-11 in [agent.acceptance](../acceptance/agent.acceptance).

## US-N1 — Bounded, predictable consumption

- As an agent, I want explicit defaults and limits so that I can plan a bounded task. TTL defaults to 24 hours and is capped at 720 hours, and macOS concurrency is capped at two leases.
- Execution accepts an explicit positive timeout without a shorter 3600-second backend cap. The caller is responsible for selecting a suitable operation budget and continuing lease renewal.

**Tests:** `TestAcquireValidation.test_ttl_bounds` (unit), `TestImagesSnapshot` (unit), `TestVmctl.test_acquire_old_flag_aliases` (integration).

## US24 — Acquire a durable machine by capability

- As an agent, I want to lease a durable machine when my task needs what a virtual machine cannot give, such as Metal 3 for an Unreal post-process evaluation, so that the task runs where it can succeed.
- I choose by the capability report, which every machine carries in the same shape, and I can name a machine or ask for a set of capabilities. See [MM-3](../arch/machines.md#mm-3-capability-report) and [CL-2](../ux/cli/list.md#cl-2-acquire-by-capability).
- A durable machine admits one lease at a time. A busy machine refuses with a reason I can wait on. See [DM-2](../arch/durable-machine.md#dm-2-one-lease-at-a-time).

**Tests:** AC-20, AC-21, AC-22 and AC-29 in [agent.acceptance](../acceptance/agent.acceptance).

## US25 — Work in a durable-machine session

- As an agent, I want to run commands and move files on a durable machine as the admin account `wezzard`, in a scratch workspace, so that I can install what my task needs in the middle of it.
- My commands see the login shell's `PATH` and the GUI session, and none of the owner's own credentials. See [DM-4](../arch/durable-machine.md#dm-4-exec).

**Tests:** AC-23 in [agent.acceptance](../acceptance/agent.acceptance).

## US26 — Claude Code and Codex reach the gateway with the machine's keys

- As an agent running Claude Code or Codex in a durable-machine session, I want both to reach the LiteLLM gateway with that machine's own key, so that the owner can track usage per machine.
- Claude Code uses `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`, never `ANTHROPIC_AUTH_TOKEN`. Codex uses a custom provider with `LITELLM_CODEX_KEY`. The gateway's base URLs come from the machine's local configuration, never from an image or a default (DC-32). No key ever appears in a response, a log or a file the service writes. See [AK](../arch/agent-keys.md).

**Tests:** AC-24 to AC-27 and AC-44 in [agent.acceptance](../acceptance/agent.acceptance).

## US27 — Work on a machine on another Mac

- As an agent on one Mac, I want to lease and work on a machine that another Mac's instance owns, so that I can use every machine on the intranet.
- Push and pull carry the bytes and their digest, because my file paths do not exist on the other Mac. See [FD-4](../arch/federation.md#fd-4-working-on-a-remote-instance).

**Tests:** AC-28 in [agent.acceptance](../acceptance/agent.acceptance).

## US36 — My actions run as the person at the desktop

- As an agent, I want every action to run in the desktop session of the machine's logged-in user, so that the keychain and the window server behave as they do for a person at that machine (DC-97).
- My action runs as a process of its own, never as a descendant of the relay agent, so it never inherits the agent's privacy permissions. Any privacy permission my task needs is my task's to arrange (DC-308, DC-309, DC-310).
- This holds on macOS virtual machines, durable Macs and Ubuntu virtual machines (DC-98). See [relay agent](../arch/relay-agent.md).

**Tests:** AC-55 to AC-60 and AC-65 in [relay.acceptance](../acceptance/relay.acceptance).

## US37 — A bad link never loses or repeats my work

- As an agent, I want my actions to survive broken, silent and slow links and a change of my own address, so that a long link between sites never loses a result and never runs an action twice.
- See [broker](../arch/broker.md) and [remote session](../arch/remote-session.md).

**Tests:** AC-61 to AC-64, AC-67 and AC-70 in [relay.acceptance](../acceptance/relay.acceptance).
