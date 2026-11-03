# Hygiene and the lab runners (HY)

## Why

- A virtual machine is clean because every lease gets a fresh clone. A durable machine cannot be cloned, so a lease on it inherits whatever earlier work left behind.
- The owner ruled that a session runs as the admin account and that hygiene is a pragmatic best effort (DC-23). Hygiene therefore tells a client what state the machine was in and what the lease changed. It does not promise a clean machine.
- The approach generalizes what the evaluation runner already does for its licensed persistent VM: a workspace, a sweep of leftover processes, and an inventory before and after ([CN-2](consumers.md#cn-2-the-evaluation-runner)).

## HY-1: Best effort, not isolation

- Hygiene has three parts: a scratch workspace ([HY-2](#hy-2-scratch-workspace)), a leftover-process sweep ([HY-3](#hy-3-leftover-process-sweep)), and an inventory before and after the lease ([HY-4](#hy-4-inventory-before-and-after)) (DC-28).
- Hygiene reports. It never refuses an acquisition, never blocks a release, and never undoes a change outside the workspace.
- A failure in any hygiene step is recorded with its reason in the lease's hygiene record.
- A step that needs a party absent at release, the relay agent's purge of the lease's records, is kept pending and finished when that party returns, not by holding the Mac. The hygiene record then says it was finished later ([DM-7](durable-machine.md#dm-7-release), DC-177).

## HY-2: Scratch workspace

- Every durable lease starts with an empty workspace, `~/machine-service/leases/<lease_id>/`, which is the default working directory and the session's `TMPDIR` ([DM-1](durable-machine.md#dm-1-a-lease-is-a-session-and-a-workspace)).
- Release removes the workspace unless the client asks to keep it ([DM-7](durable-machine.md#dm-7-release)).

## HY-3: Leftover-process sweep

- At release, after the lease's own process groups are stopped, the service ends the processes that the lease left running.
- A process is a leftover when it runs as the operator's account, it started after the lease began, and one of these holds:
  - It belongs to a process session that the lease started, or it descends from one. A command that the relay agent ran for the lease counts its process group as such a session ([DM-4](durable-machine.md#dm-4-exec)); the relay agent itself and its other commands share its session and are never matched.
  - Its environment carries `MACHINE_SERVICE_LEASE_ID` with this lease's ID. macOS 27 does not let a process read another process's environment, so there this rule finds nothing, and a process that left the lease's sessions after its parents exited is not swept (DC-48).
  - It is a regular application, one with a Dock presence, that the lease launched: it was not running in the inventory before the lease, and its launchd job in the user's GUI domain (`application.<bundle>.<n>.<n>`) was started with `MACHINE_SERVICE_LEASE_ID` set to this lease's ID. Applications started through Launch Services, for example with `open`, are children of launchd and belong to no session of the lease, but `open` hands the caller's environment to the job it creates, so the lease's own launches carry the variable. An application that was already running is only activated by `open` and keeps its own job, and one that the owner or another program opened carries no such variable, so the rule never ends an application the lease did not launch (DC-73).
- The job's environment is read with `launchctl print`; only `MACHINE_SERVICE_LEASE_ID` is parsed out, in memory, and nothing else of the output is logged or kept, because a job inherits whatever its caller's environment held.
- A leftover application is first asked to quit with an Apple event (`NSRunningApplication.terminate`, sent through `osascript`) and is given 10 seconds. A leftover process then receives `SIGTERM`, and after 10 more seconds `SIGKILL`. The hygiene record names the last signal: `QUIT`, `TERM` or `KILL`.
- An application that another program launches for the lease, such as CuaDriver's own `launch_app`, carries that program's environment and is not attributed to the lease.
- The sweep never ends these processes, whatever the rules above say:
  - The CI runners and every process that descends from them ([HY-5](#hy-5-the-lab-runners-hold-the-lease)).
  - Any launchd job's own process that the lease did not start, such as Consul, the service-catalog registrar and machine-service itself.
  - Finder, the Dock, SystemUIServer and the other processes of the login session.
- The rules are applied to one snapshot of the processes taken before any signal, so stopping a session never hides a descendant that left it.
- The hygiene record lists every swept process with its PID, executable, start time, the rule that matched it (`session`, `descendant`, `environment` or `application`), the last signal, and whether it ended.

## HY-4: Inventory before and after

- The service takes one inventory at acquisition and one at release, after the sweep.
- An inventory records:
  - The running processes of `wezzard`, with executable and start time.
  - The running regular applications.
  - The installed applications in `/Applications` and `~/Applications`, with bundle version.
  - The launchd jobs in `~/Library/LaunchAgents`, `/Library/LaunchAgents` and `/Library/LaunchDaemons`.
  - The Homebrew formulae and casks, the global npm packages, and `wezzard`'s crontab.
  - The top-level entries of the home folder, `~/Library/Application Support` and `~/Library/Preferences`, with their modification times.
  - The free disk space and the `software` section of the capability report ([MM-3](machines.md#mm-3-capability-report)).
- Each part of the inventory has its own timeout of 60 seconds. A part that fails or times out is recorded as missing, and the others are kept.
- The global npm packages are recorded as `{name: version}` from the session's `npm ls --global --depth=0 --json`. A crontab entry is recorded as its schedule and its program, keyed by the SHA-256 of its whole line: a change shows, and an argument, such as a token written into a crontab line, never reaches the hygiene record that any LAN client can read (DC-74).
- The service compares the two inventories and records what was added, removed and changed. The CI runners' processes and files are marked with their origin, so a client does not attribute a CI job's work to the lease.
- The lease record keeps `inventory-before.json`, `inventory-after.json` and `changes.json` under the lease's state folder. The release response carries `changes.json` and the paths of the full inventories.
- `GET /machines/<name>/leases/<lease_id>/hygiene` returns the hygiene record of a current or recent lease for 30 days. Its `relay_records` changes once after release, when a pending purge is confirmed (DC-177).

## HY-5: The lab runners hold the lease

- **Superseded** (DC-1xx-LAB-21): lab runners run in ephemeral one-job VMs on the VM hosts, and the host runners described below were removed from the durable Macs (DC-1xx-LAB-22). The workflow `lab.yml` is skipped until the ephemeral runners exist. The rest of this section records how the host runners worked; `ci_runner` and the sweep still recognize any `~/actions-runner` or `~/actions-runner-<name>` folder.

- The durable Macs at 192.0.2.4 and 192.0.2.5 are the lab runners of this repository's lab workflows, and no longer readycheck-dev runners (DC-1xx-LAB-1). Each runs a GitHub Actions runner as the operator's account, from `~/actions-runner-lab`, registered with the labels `self-hosted`, `macOS`, `ARM64` and `lab`. It runs as the per-user LaunchAgent `com.github.actions.runner.lab` in the desktop session. macOS's local network privacy attributes every LAN connection of a job to the runner's own program (`Runner.Listener` and `Runner.Worker`, identifier `apphost-…`), and the operator allows it once on each Mac (DC-1xx-LAB-14). The runner's own executables are signed ad hoc, so a grant names their exact build: the runner never updates itself (`--disableupdate`), and the operator installs a new version, and grants it again, deliberately. Only lab jobs ask for `lab`; every other job runs on GitHub-hosted runners.
- **A lab job is a client of machine-service.** The runner's job-started hook (`ACTIONS_RUNNER_HOOK_JOB_STARTED`) acquires its own Mac's durable lease through `machctl acquire --machine <the Mac> --purpose ci-<run id> --env none`, with the Mac's name read from the local instance at run time, and its job-completed hook releases it, with the sweep, the inventory and the relay agent's purge of a release ([DM-7](durable-machine.md#dm-7-release)). When the Mac is leased, the hook fails the job at once with the lease's purpose and end, and the job never starts its steps: a lab job never shares the desktop with a lease, and a lease never starts while a lab job runs, because the job holds the lease (DC-1xx-LAB-2). This supersedes DC-25's coexistence for these Macs.
- The lease has a TTL of 2 hours and is renewed every 10 minutes for as long as the job's `Runner.Worker` runs, so a job of any length keeps it, and a runner that dies leaves a lease that ends at its TTL. A lease that a job left, because its completed hook never ran, is released by the runner's next job-started hook. The hooks keep the current lease in `~/actions-runner-lab/_lab/lease.json` and each released lease's hygiene record in `_lab/hygiene/<lease_id>.json`, and print nothing that names the Mac, because a hook's output is part of the job's public log (`rust/scripts/lab-runner.sh`). The runner prints a hook's path in that log before any step can mask it, so it calls the hooks through links in `/var/tmp/lab-runner-hooks/`, outside the home folder.
- The sweep never ends a runner's processes ([HY-3](#hy-3-leftover-process-sweep)). A runner is any folder `~/actions-runner` or `~/actions-runner-<name>`: its executables, a script there run by an interpreter, and their descendants. The workspace and the inventory never touch those folders, and the inventory marks their processes with the origin `ci`.
- `ci_runner` in the capability report ([MM-3](machines.md#mm-3-capability-report)) says whether a runner is configured in any of those folders (`present`, `name`, the first configured runner's) and whether a job is running (`busy`). On a lab runner, a running job holds the lease, so a busy runner shows as the lease of purpose `ci-<run id>`; `busy` stays for a runner without the hooks. A client never needs to wait for an idle runner: it waits for the lease, as for any lease ([DM-2](durable-machine.md#dm-2-one-lease-at-a-time)).
- During a lease, the service still samples whether a CI job runs once a minute, with every GC pass, and also at acquisition, on `POST /gc` and at release. The hygiene record's `ci_busy_intervals` lists `{from, to}` for each stretch a job was seen running, an interval still open at release closing there, and `ci_sampling` gives the interval and the number of samples. A job that starts and ends between two samples is not seen (DC-75). On a lab job's own lease the interval is the job itself; on any other lease it stays empty unless a runner without the hooks ran a job.
- The old readycheck-dev runner's folder `~/actions-runner` and its LaunchAgent `com.github.actions.runner` are left as they are; the runner is no longer configured there. Its nightly `com.wezzard.cleanup-runner` job, which removes `~/actions-runner/_work/*/*/target`, is likewise left alone.
- **Only the owner's changes run on the lab runners** (DC-1xx-LAB-3, DC-1xx-LAB-12). A lab job runs on a push to `claude/relay-agent` of this repository and on a dispatch, never on a pull request. Only the relay acceptance runs there: no test suite, whose default tests reach the LAN, the installed instance and the operator's desktop, so ordinary CI stays on GitHub-hosted runners. Every command of a lab job runs in the job's own environment, with no inherited `VM_ENVIRONMENT_*`, `MACHINE_SERVICE_*`, `CODESIGN_IDENTITY` or `ZDOTDIR`, a `HOME` and `TMPDIR` under the runner's work folder, and its own Rust toolchain there, so no global tool changes. The repository requires approval before a workflow of a pull request from a fork runs, for every outside contributor. A lab workflow's token is limited to `contents: read`, and the workflow uses no secret.
- SSH serves only the lab's test setup, through restricted keys, never the work under test (DC-1xx-LAB-19): a lab job reaches another host's acceptance setup through that host's gate, and everything under test goes through machine-service and the relay.
- One lab job runs at a time, in the concurrency group `lab`. A job holds its own Mac's lease, so the relay acceptance's durable cells target the other durable Mac and its virtual machine cells the VM hosts, all found from `machctl list` at run time, and a job never waits on its own lease ([TL-7](test-layers.md#tl-7-relay-acceptance)).

## HY-6: Free disk

- Each Mac mini at 192.0.2.4 and 192.0.2.5 has a 228 GiB system volume with about 125 GiB free. A task that installs an engine can use a large share of it.
- The capability report always carries the free disk space. Below the configured warning level, `MACHINE_SERVICE_DISK_WARN_GB` in GiB, the machine record's `warnings` carries `{"code": "low-free-disk", "message": "low free disk: <n> GiB free, below the warning level of <n> GiB"}`. The warning never makes the machine unavailable.
- The level is 30 GiB when the variable is unset, about the space one engine installation takes; `0` turns the warning off, and a value that is not a number stops the daemon, so a typo never silences the warning (DC-76).
