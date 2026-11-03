# Consumers (CN)

## Why

- Two programs lease machines today: mcp-relay, which runs agents' commands and scripts in guests with recorded evidence, and the AnyDict evaluation runner, which runs its rows through mcp-relay and, for its Unity rows, in one persistent licensed VM over SSH.
- Both assume a loopback vm-service and a Tart guest. This section states what each must change to consume durable machines and remote instances. The changes are built and tested in each consumer's own repository, as DC-4 set for mcp-relay.
- Moving to the new names and the `/machines` API is a separate, earlier step: every caller makes it in the coordinated switch of [MG-3](rename.md#mg-3-one-coordinated-switch). Consuming durable machines builds on that switch.
- Only 192.0.2.4 and 192.0.2.5 expose durable machines (DC-39). 192.0.2.3 offers only virtual machines.

## CN-1: mcp-relay

- **What it does today.** mcp-relay accepts only a loopback vm-service origin, `MCP_RELAY_URL`, and refuses non-loopback servers, redirects and physical targets. Its instructions say that it never targets a physical or local display. It acquires a VM by image, stages its runtime per guest into `/var/tmp/<UTC time>-mcp-relay-<task>/` (`receiver.mjs`, `support/`, `workspace/`), needs only Node.js in the guest, and takes snapshots only when the image's catalog lists cua-driver. It waits for capacity when vm-service answers `macOS VM limit reached`.
- **Origins.** The relay accepts, besides loopback, the address of an instance that discovery finds in the mode `MACHINE_SERVICE_DISCOVERY` chooses, Bonjour by default or Consul, once its `/health` confirms the instance ID ([FD-2](federation.md#fd-2-discovery), DC-84, DC-85). It keeps refusing redirects, credentials in the URL, and any other non-loopback origin.
- **Choosing a machine.** `relay_acquire` takes either `image`, as today, or `machine`, the name of a durable machine. It may also take `require`, a list of capabilities such as `metal3` or `unreal`, which the relay resolves to a machine from the merged list ([FD-3](federation.md#fd-3-aggregation)). The relay picks a durable machine only when the caller names one or requires a capability that no virtual machine offer has.
- **Lease operations.** Every operation on a durable machine passes the lease's `lease_id` ([MM-2](machines.md#mm-2-identity)). Push and pull to a remote instance use the byte-carrying endpoints ([FD-4](federation.md#fd-4-working-on-a-remote-instance)).
- **Renames.** The relay saves the owner's machine ID at acquisition, confirms the owner by it after its own restart, and keeps addressing the lease by its granted name ([MI-5](identity.md#mi-5-clients)). A 404 for a durable machine never proves that the lease ended: the machine outlives its leases, so the relay asks the owner again before it forgets the lease.
- **Staging.** The relay stages its runtime per session, as today. On a durable machine its root is the lease's workspace, which acquisition returns, instead of a folder under `/var/tmp/`. The service removes the workspace at release, after the relay has extracted its evidence.
- **Snapshots.** On a durable machine the relay takes snapshots when the capability report lists `software.cua_driver` and `cua-driver permissions status --json`, run in the lease, answers `source.attribution: "driver-daemon"` with `screen_recording: true`, instead of reading an image catalog. It reads no `permissions` of machine-service, because the `cua-driver serve` LaunchAgent, not machine-service, captures the screen (DC-82, DC-83).
- **Finish and release.** On a durable machine, `relay_finish` and `relay_release` end the session ([DM-7](durable-machine.md#dm-7-release)) instead of destroying a VM. The relay keeps the release report's hygiene record in the evidence package.
- **Capacity.** The relay treats `durable machine leased` like the macOS VM limit: it waits, up to `MCP_RELAY_CAPACITY_WAIT`.
- **Instructions.** The relay's instructions say that a durable machine is a real Mac on the home network, that it is not isolated from the home network ([DM-9](durable-machine.md#dm-9-what-a-durable-lease-does-not-offer)), and that a lab job holds the Mac's lease while it runs, so a lease never shares the desktop with one ([HY-5](hygiene.md#hy-5-the-lab-runners-hold-the-lease)); the relay's instruction text that a CI job may share the desktop predates DC-1xx-LAB-2.
- **Selected environments.** The selected-environment profile keeps naming a loopback instance, and its fingerprint check keeps applying to the local instance.

## CN-2: The evaluation runner

- **What it does today.** Each row runs in a fresh clone of its `vm` image through mcp-relay (AnyDict decision D36). A Unity row instead runs in the persistent clone `pilot-mac26g-unity`, where the owner activated Unity, through `evaluations/licensed-vm.mjs` over pilot-images' `vssh` and `vscp`. That path keeps its own lock, workspace under `~/interope-eval/<task>/`, inventory before and after, and sweep. `guest-row.zsh` grants the SSH context its privacy permissions in a clone, where SIP is off, and only checks them in the persistent VM. The Unreal rows are blocked.
- **Durable rows.** A row may name a machine requirement instead of a `vm` image, for example `{"kind": "durable", "require": ["metal3", "unreal"]}`. The runner passes it to `relay_acquire` and the row runs on the chosen durable machine through the relay, exactly as a clone row runs today.
- **Hygiene.** On a durable machine the service's lease provides the lock, the workspace, the inventory and the sweep ([HY](hygiene.md)). The runner records the release's hygiene record in the row's `vm.json`, instead of computing its own as `licensed-vm.mjs` does.
- **Credentials.** `guest-row.zsh` already accepts `ANTHROPIC_API_KEY` for Claude Code. For Codex it accepts `LITELLM_CODEX_KEY` with the session's `CODEX_HOME` provider ([AK-4](agent-keys.md#ak-4-codex)), so Codex rows, which are refused in clones today for lack of credentials, can run on durable machines.
- **Permissions.** On a durable machine, SIP is on, so `guest-row.zsh prepare` never writes the permission store. It checks the grants a row needs and refuses the row when one is missing, as it already does in the persistent VM. Screen capture and input go through the cua-driver daemon, whose own grants apply, so the runner reads them with `cua-driver permissions status --json`, as the relay does (CN-1, DC-82). machine-service's `permissions` describe only its own executable ([DM-4](durable-machine.md#dm-4-exec)).
- **Unreal first.** The Unreal post-process row needs Metal 3, which no Tart VM offers (DC-27). Unreal Engine and its Epic sign-in can be installed once on a durable machine and stay there, which a disposable clone cannot keep.
- **Recording.** A recorded walkthrough captures the Mac's whole screen. The runner records the CI busy intervals from the hygiene record; on the lab runners a CI job cannot share the desktop during the row, because a lab job holds the lease (DC-1xx-LAB-2), so the intervals stay empty unless a runner without the lease hooks ran a job.

## CN-3: What a durable machine needs installed

- pilot-images' `metal/bootstrap.zsh` provides the software baseline of a Mac mini with the same guest phases as the VM images. The minis at 192.0.2.4 and 192.0.2.5 were found with this state on 2026-10-20:

| Need | 192.0.2.4 and 192.0.2.5 today | What closes the gap |
| --- | --- | --- |
| A logged-in GUI session for `wezzard` | `wezzard` is the console user and the auto-login user; display sleep is off | nothing |
| Node.js, which the relay's staging needs | installed by Homebrew at `/opt/homebrew/bin/node`, on the login shell's `PATH` only | the login environment capture of [DM-4](durable-machine.md#dm-4-exec) |
| cua-driver, for snapshots and desktop automation | `CuaDriver.app` 0.19.3 is installed; its command-line tool is not on `PATH` | a link on `PATH`, or the relay names the tool inside the app bundle |
| Claude Code and Codex | not installed | install both as part of the machine's baseline |
| Agent keys | not present | the owner writes the two key files ([AK-1](agent-keys.md#ak-1-key-files)) |
| Privacy permissions for CuaDriver | not granted; SIP is on, so the permission store cannot be written | the owner grants Screen Recording and Accessibility to CuaDriver (`com.trycua.driver`) once in System Settings; machine-service is never granted Screen Recording (DC-82) |
| Tart and images | not installed | not needed for durable machines |
| Durable exposure | not installed | `machine-service-install --expose-durable on` |

- The owner's grants must be repeated only when an executable's signing identity changes. machine-service is therefore signed with one stable identity on every Mac ([DM-4](durable-machine.md#dm-4-exec)).
