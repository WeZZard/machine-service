# Test layers (TL)

Each behaviour is tested on the lowest layer that contains everything the behaviour depends on. A higher layer repeats a check only when it adds something real: a real process, a real network or a real VM. Acceptance criteria are in [acceptance](../acceptance/index.md).

| Layer | Guest link (GS) | Client link (RS) | Runs in |
| --- | --- | --- | --- |
| TL-1 Unit | frames, CRC-32, the agent's request loop, client outcomes | session state machine, replay, deduplication, heartbeat timers | `cargo test --workspace` |
| TL-2 Component integration | the real agent binary as a local process; the real daemon with a fake `ssh` that runs the agent locally | the real daemon and a Rust test client through a fault proxy | `cargo test --workspace` |
| TL-3 Network lab | none | real silent packet loss, address change, an HTTPS-only proxy | test VMs with network conditioning, opt-in |
| TL-4 Live VM | both channels against macOS 26 and Ubuntu 24.04 VMs | none | second instance, `#[ignore]` |
| TL-5 End to end | Pi's traced Reminders run against the 2026-10-07 baseline | the same run with added network latency | Pi repository, opt-in |
| TL-7 Relay acceptance | the relay agent in the desktop session | the broker through the network rig | real machines, 6 cells × 4 network profiles, opt-in |

## TL-1: Unit

- Frames: encode and decode every frame type; reject a bad magic, an oversized length, a short read and every single-bit flip of the checksum's input.
- Agent loop over in-memory pipes: exec output and exit codes, a script on standard input, a timeout, `cancel`, write then read with SHA-256, a directory read answered by `is_directory`, and exit when input closes.
- Client outcomes: for each row of [GS-5](guest-session.md#gs-5-failure-outcomes-at-most-once), a scripted fake stream that fails at that point produces exactly the outcome in the table, and the number of requests sent is counted.
- Remote session state machine: a randomized test drops, duplicates and delays envelopes, cuts connections at random points and resumes. Every call completes exactly once or is reported uncertain, and no operation runs twice.

## TL-2: Component integration

- Guest link: the `guest-session` crate's tests start the real `guest-session-agent` binary as a child process and drive it through the client. Faults: kill the agent during an exec, flip a byte in its output stream, and move a 64 MiB file each way.
- Guest link through the daemon: the `vm-service` tests start the real daemon with a fake `ssh` that runs its remote command locally, with `HOME` set to a temporary guest root. The daemon uploads the agent, opens one session and serves many execs, pushes and pulls over it. The fake `ssh` counts how many connections were opened.
- Client link: the `vm-service` tests start the real daemon and connect a Rust test client through an in-process fault proxy. Faults: reset, a silent hang that keeps the connection open, latency and jitter, a reconnect from a new source port, and a daemon restart.

- Machine identity (AC-50 to AC-54): real daemon processes use separate ports, state directories and configuration directories. `MACHINE_SERVICE_NAME_FILE` supplies the name, and `MACHINE_SERVICE_NAME_POLL_MS=50` makes the same observation path run every 50 milliseconds. Tests edit the file without changing the Mac's host name. `MACHINE_SERVICE_DISCOVERY_SCOPE=local-only` confines both Bonjour advertisement and client browsing to this Mac. Tests of a single local listing use `machctl list --local`; Consul tests use their own loopback registrar fixture. Durable fixtures use the existing debug-only `MACHINE_SERVICE_DURABLE_LOCK_DIR` to isolate their lock.

## TL-3: Network lab

- The lab runs in test VMs leased from vm-service, where the test has root. The client runs in one VM and the daemon's remote session listener runs in another, so the faults sit on a real network path between them.
- macOS VMs condition the network with the system's packet filter and traffic shaper (`pfctl` and `dnctl`, the mechanism under Network Link Conditioner): delay, loss, and a silent drop of all packets. Linux VMs use `tc netem` and `iptables`.
- An address change moves the client VM to another address during a session. An HTTPS-only path is a proxy in a VM that allows only `CONNECT` to port 443.
- The lab holds at most one macOS VM at a time, because a host runs at most two macOS guests and other work shares them. Faults that need no macOS behaviour run in Linux VMs.
- An agent runs the lab's test plan and records results and timings under `test-evidence/`.

## TL-4: Live VM

- Live tests are `#[ignore]` with a reason, as `rust/CONVENTIONS.md` requires. They run against second instances built from the worktree, each on its own port and state directory, and send commands directly to their own test VMs.
- Two instances run at once, one per guest link (6250 for the session link, 6251 for `ssh`), so a parity case compares two leases of one image without restarting a daemon. `rust/scripts/live-instance.sh` starts each instance in its own session: stopping the shell that started it must not signal the `tart run` processes of its VMs.
- Every live test checks that the installed daemon on 6240 kept its process and holds no test lease (AC-18).
- The conformance suite runs the same cases over the SSH stdio channel and the Tart exec channel, on a macOS 26 image and an Ubuntu 24.04 image with the Tart guest agent installed.
- Cases: exec parity with the current `ssh` link (output, exit code, `PATH`, user and home); a 64 MiB output and file each way with matching SHA-256; the guest network turned off during a session; a VM restart and a stale Tart control socket; release during a running command; round-trip time per operation.
- The report records each case's result and timings per channel, image and Tart version under `test-evidence/`, which is ignored by git.

## TL-5: End to end

- Pi's traced Reminders run (`PI_SECRETARY_VM_USE_TRACE`) is repeated with mcp-vm-relay on the new links. Its step intervals are compared with the 2026-10-07 baseline of a 4.37 s mean between actions, with and without added latency on the client link.
- Each step interval is split into the model's time, the guest's own work and the transport's time. Only the transport's time is in scope; the model's time and the guest's work set the floor.
- The work is done when the transport's time is the smallest of the three parts, both without and with added latency on the client link. The run's report states the floor, so the result is read against what the system can reach rather than against a guessed number.

## TL-6: Live durable machine

- Durable-machine behaviour that needs a real Mac is tested on the Mac itself, with a second instance on another port, with its own state and configuration folders and with durable exposure turned on, as TL-4 does for VMs. Its configuration folder holds test agent keys and a test `endpoints.json`.
- On 192.0.2.3 the installed instance keeps exposure off (DC-39), so the durable lease is proved there only through second instances (DC-40). On 192.0.2.4 and 192.0.2.5 the installed instance exposes the Mac.
- Every instance on a Mac shares the host-wide lease lock ([DM-2](durable-machine.md#dm-2-one-lease-at-a-time)), so two second instances prove the lock across instances, and a live test on a mini can never lease the Mac while the installed instance holds it, or the other way round.
- Cases: an acquisition while the machine is leased; exec in the GUI session with the expected `PATH` and no owner credential variables; the agent keys reaching Claude Code and Codex without appearing in any response or log; a leftover application and a leftover background process swept at release while a stand-in CI process survives; the inventory recording an installed Homebrew formula; and a release during a running command.
- The CLI's federated list runs against the real Consul agent and lists every registered instance, including one that is stopped for the test.
- TL-6 never pauses the CI runner and never changes the installed instance, as AC-18 requires.

## TL-7: Relay acceptance

- The relay acceptance suite is the final acceptor of the [relay agent](relay-agent.md), the [broker](broker.md) and the [transport contract](transport-contract.md) (DC-103).
- It runs on real machines with the real relay agent, the real broker and a real client, and it turns sshd off in every guest after acquisition and before the first action.
- It does not test SSH.

### The network rig

- Every connection between a client and the broker, and between a relay agent and the broker, passes through a chain of two proxies (DC-104), with the rig's own reset stage at each end of the chain (DC-137).
- The first proxy is Toxiproxy, which injects faults at the TCP level: latency with jitter, a bandwidth limit, a blackhole (`timeout`), a reset (`reset_peer`), a close after a number of bytes (`limit_data`) and fragmentation (`slicer`).
- The second proxy is mitmproxy with an edge script in this repository, which ends TLS and opens a new TLS connection, closes a WebSocket that is idle for 100 seconds, and closes a connection that sends a WebSocket message larger than 1 MiB.
- The reset stage is a TCP relay in the `network-rig` crate, with one hop in front of Toxiproxy and one between mitmproxy and the broker. It cuts a chosen open connection, or every one, with a TCP RST toward the client, the broker or both, by closing that side's socket with a zero linger time. The hop's other socket closes normally, so the rest of the chain sees an ordinary close, and a reset that reaches a hop from one side is passed on as a reset to the other.
- The stage pairs a client's connection with the broker connection that the edge opens for it by the random of the client's TLS ClientHello, which the front hop reads as it passes and the edge writes to the run's `edge-pairs.jsonl` with its source port.
- The stage adds 40 to 70 µs to a round trip over loopback at the median, measured by the self-test `the_reset_stage_adds_negligible_latency` on macOS and Linux.
- Both proxies and the stage run without root on the client's machine, the proxies at pinned versions, and the broker is reachable only through the chain's one port and only by a host name.
- Faults follow a seeded schedule, so a failing run can be replayed exactly, and a test can also set a fault at an exact moment through the rig's `Control`, which drives Toxiproxy's HTTP API and the reset stage.
- The rig is the `network-rig` crate. It pins Toxiproxy 2.13.0 and mitmproxy 12.3.1 for macOS arm64 and Linux arm64 and x86_64, and fetches them into a cache outside the repository, checking each SHA-256 before use. The edge script is `rust/crates/network-rig/edge/edge.py`.
- A client is told the broker's URL, `wss://broker.relay-rig.test:<port>/relay`, the chain's port to resolve the name to, and the CA bundle of the edge's authority. It resolves the name itself, so no system setting changes. The edge answers `421` to any other name and to an address, and the broker's own certificate never reaches the client.
- The edge ends TLS under every profile, because AC-66 and AC-68 run under N0. Its idle close counts every byte in either direction, WebSocket pings included, and its message limit closes the WebSocket with code 1009; a message of exactly 1 MiB passes.
- A reset is Toxiproxy's `reset_peer`: it sends a TCP RST to every connection dialled while it is armed, and cuts an open connection without a RST when it next carries data. A cut is the stage's: an open connection receives a TCP RST at once, on the side chosen. The edge turns a reset on one of its legs into a WebSocket close with code 1000 on the other, as an edge that ends TLS does. A blackhole drops data without closing anything, and closes the connection when it ends.
- An address change closes every connection, and the broker sees each new connection come from a new source address and port on Linux, where the stage's back hop moves to the next address of `127.0.0.0/8`, which needs no root. On macOS, whose loopback has one address without root, only the source port changes. When the broker is on another machine, which a loopback source cannot reach, only the source port changes as well (DC-192). The rig records which in `rig.json` as `address_change`.
- `NETWORK_RIG_SEED` replays a seed. Each run records its profile, seed and schedule in `rig.json`, and the cell runner writes each cell's results to `report.json` under `<evidence>/<cell>/<profile>/`. Toxiproxy's latency jitter is not seeded, so a replay repeats the faults but not each packet's delay.
- The rig's self-tests are opt-in: `cargo test -p network-rig --test rig_live -- --ignored`. They drive a WebSocket echo server under TLS in place of the broker.
- A relay agent's chain runs on its target's host as `network-rig serve`, in front of the broker, and `network-rig ctl` sets its faults. Guests reach it on their host's address on the guest network (`MACHINE_SERVICE_RELAY_GUEST_PORT`), and its edge, which signs with the broker's authority that the agents trust, serves those addresses as well as its name (DC-184, DC-185).
- CI runs the self-tests on every change on Ubuntu x86_64, Ubuntu arm64 and macOS arm64, the three platforms the rig pins, and caches the pinned tools by their versions and hashes (DC-138).

```mermaid
flowchart LR
    Client -->|wss://name| Front[reset stage, front hop: RST toward the client]
    Front --> Toxi[Toxiproxy: TCP faults]
    Toxi --> Mitm[mitmproxy edge script: TLS, idle close, message size]
    Mitm --> Back[reset stage, back hop: RST toward the broker, source address]
    Back --> Broker[broker in machine-service]
    Agent[relay agent] -->|through its own chain| Broker
```

### Network profiles

| Profile | What the rig does | Why |
| --- | --- | --- |
| N0 Clean | The chain is in place and injects no fault: TLS ends at the edge and the broker is reached by name, with no idle close and no message limit. | It gives a baseline and the speed measurement. |
| N1 Edge | It enforces TC-1, TC-3, TC-5, TC-6 and TC-8 at all times: an idle close at 100 seconds and a limit of 1 MiB per message. | It behaves like a Cloudflare edge. |
| N2 Long-haul | It adds TC-7 to N1: 300 ms to 1 s of latency with jitter (325 ms ± 175 ms each way), a bandwidth limit of 4096 KB/s each way and fragmentation into pieces of 256 to 1792 bytes. | It behaves like a link between continents. |
| N3 Chaos | It adds TC-2 and TC-4 to N2: random resets, cuts, blackholes, data limits and address changes on a seeded schedule, the first after 10 seconds and then one every 20 to 60 seconds after the previous one ends. | It imposes the worst case of the contract. |

The profiles are JSON files in `rust/crates/network-rig/profiles/`.

### Cells

- The suite runs in 6 cells: a client on macOS or on Ubuntu, against a macOS virtual machine, a durable Mac or an Ubuntu virtual machine.
- Each cell runs under the 4 profiles, so a full acceptance is 24 runs of the suite.
- AC-55 to AC-60, AC-66 and AC-68 run under every profile.
- AC-61 to AC-65, AC-67 and AC-69 run under N1, N2 and N3.
- AC-70 measures speed under N0 and N2.

### Running the suite

- The suite is the `relay-acceptance` crate: a Rust test client, the real target and the scenarios. `relay-acceptance run --cell <cell> --profile <N0..N3> --instance http://<name>:<port> [--image <image>] [--host-shell <command> --host-script <path>] --evidence <dir>` runs one cell under one profile on the machine of its client platform, and `relay-acceptance summary <dir>` prints the matrix. `cargo test -p relay-acceptance --test live -- --ignored` runs the same from `RELAY_ACCEPTANCE_*` variables.
- On each target host, `scripts/relay-acceptance-host.sh` runs the second instance with the relay link (`start`, `stop`, `restart`, `clean`) and the agents' chain (`rig-start <profile> <seed>`, `rig-stop`, `rig-ctl <request>`), with everything in one folder. The suite runs it through `--host-shell`, which the operator gives, to start the chain with the cell's profile, to cut the agents' connections and to restart the broker.
- An Ubuntu client cell runs in a leased ubuntu2404 VM: `scripts/relay-acceptance-ubuntu-client.sh` leases it, puts the static Linux build in it, runs the cell there and brings the evidence home.
- Each scenario reports named checks. A check that waits for something not built yet or not in the images yet is `blocked`, with the reason, and is not a failure (DC-187).

### Coverage of the contract

| Rule | Proven by | The suite fails when |
| --- | --- | --- |
| TC-1 | every scenario under N1 to N3 | the stack uses another port, UDP or an inbound connection |
| TC-2 | AC-61, AC-64 and N3 | an operation runs twice or a result is lost |
| TC-3 | AC-69 | heartbeats are missing or too far apart |
| TC-4 | AC-63 and N3 | a session is tied to an address or a connection |
| TC-5 | AC-66 under N1 to N3 | a credential depends on a client certificate |
| TC-6 | AC-68 under N1 to N3 | discovery or a client needs an IP address |
| TC-7 | AC-56 to AC-59 under N2 and N3 | a timeout assumes a fast link |
| TC-8 | AC-57 and AC-58 under N1 to N3 | data is sent in a message larger than 1 MiB |
