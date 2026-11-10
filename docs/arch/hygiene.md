# Hygiene and CI coexistence (HY)

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
  - It belongs to a process session that the lease started, or it descends from one. A command of the lease is a launchd job whose label the lease record names, and the shell that launchd started for it leads its process group, which counts as such a session (its own session is launchd's, so the rule goes by the recorded group, [DM-4](durable-machine.md#dm-4-exec), DC-312). The exclusion of launchd jobs' own processes never spares a job the lease record names; the relay agent itself is never matched.
  - Its environment carries `MACHINE_SERVICE_LEASE_ID` with this lease's ID. macOS 27 does not let a process read another process's environment, so there this rule finds nothing, and a process that left the lease's sessions after its parents exited is not swept (DC-48).
  - It is a regular application, one with a Dock presence, that the lease launched: it was not running in the inventory before the lease, and its launchd job in the user's GUI domain (`application.<bundle>.<n>.<n>`) was started with `MACHINE_SERVICE_LEASE_ID` set to this lease's ID. Applications started through Launch Services, for example with `open`, are children of launchd and belong to no session of the lease, but `open` hands the caller's environment to the job it creates, so the lease's own launches carry the variable. An application that was already running is only activated by `open` and keeps its own job, and one that the owner or another program opened carries no such variable, so the rule never ends an application the lease did not launch (DC-73).
- The job's environment is read with `launchctl print`; only `MACHINE_SERVICE_LEASE_ID` is parsed out, in memory, and nothing else of the output is logged or kept, because a job inherits whatever its caller's environment held.
- A leftover application is first asked to quit with an Apple event (`NSRunningApplication.terminate`, sent through `osascript`) and is given 10 seconds. A leftover process then receives `SIGTERM`, and after 10 more seconds `SIGKILL`. The hygiene record names the last signal: `QUIT`, `TERM` or `KILL`.
- An application that another program launches for the lease, such as CuaDriver's own `launch_app`, carries that program's environment and is not attributed to the lease.
- The sweep never ends these processes, whatever the rules above say:
  - The CI runner and every process that descends from it ([HY-5](#hy-5-coexistence-with-the-ci-runner)).
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
- The service compares the two inventories and records what was added, removed and changed. The CI runner's processes and files are marked with their origin, so a client does not attribute a CI job's work to the lease.
- The lease record keeps `inventory-before.json`, `inventory-after.json` and `changes.json` under the lease's state folder. The release response carries `changes.json` and the paths of the full inventories.
- `GET /machines/<name>/leases/<lease_id>/hygiene` returns the hygiene record of a current or recent lease for 30 days. Its `relay_records` changes once after release, when a pending purge is confirmed (DC-177).

## HY-5: Coexistence with the CI runner

- The readycheck-dev GitHub Actions runner on 192.0.2.4 and 192.0.2.5 runs as the operator's account, as the LaunchAgent `com.github.actions.runner`, from `~/actions-runner`. machine-service never pauses, stops or reconfigures it (DC-25).
- A lease may start while a CI job runs, and a CI job may start during a lease. They share the CPU, the memory, the GPU, the disk and the desktop.
- `ci_runner.busy` in the capability report says whether a job is running when the client asks ([MM-3](machines.md#mm-3-capability-report)). A client that measures performance, or that drives the desktop, can prefer an idle machine. A client that must wait for an idle machine does so itself; the service never makes a busy runner a refusal.
- During a lease, the service samples whether a CI job runs once a minute, with every GC pass, and also at acquisition, on `POST /gc` and at release. The hygiene record's `ci_busy_intervals` lists `{from, to}` for each stretch a job was seen running, an interval still open at release closing there, and `ci_sampling` gives the interval and the number of samples. A job that starts and ends between two samples is not seen (DC-75). A client can then judge a measurement taken while a job ran.
- The sweep, the workspace and the inventory never touch `~/actions-runner`. The runner's nightly `com.wezzard.cleanup-runner` job, which removes `~/actions-runner/_work/*/*/target`, is likewise left alone.

## HY-6: Free disk

- Each Mac mini at 192.0.2.4 and 192.0.2.5 has a 228 GiB system volume with about 125 GiB free. A task that installs an engine can use a large share of it.
- The capability report always carries the free disk space. Below the configured warning level, `MACHINE_SERVICE_DISK_WARN_GB` in GiB, the machine record's `warnings` carries `{"code": "low-free-disk", "message": "low free disk: <n> GiB free, below the warning level of <n> GiB"}`. The warning never makes the machine unavailable.
- The level is 30 GiB when the variable is unset, about the space one engine installation takes; `0` turns the warning off, and a value that is not a number stops the daemon, so a typo never silences the warning (DC-76).
