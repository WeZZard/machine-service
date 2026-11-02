# Index of acceptance criteria

## Table of Contents

- Agent
- Orchestrator
- Operator
- Relay

Each scenario names the [test layer](../arch/test-layers.md) that runs it. Stories are in [user stories](../user-stories/index.md).

## Agent

[agent.acceptance](agent.acceptance)

1. AC-1: Many operations on one lease use one guest session (US19, TL-2).
2. AC-2: Guest operations over a guest session match the ssh link (US19, TL-2, TL-4).
3. AC-3: Transferred bytes arrive intact or the request fails (US19, TL-1, TL-2, TL-4).
4. AC-4: A guest session lost during an exec reports uncertain and never reruns it (US19, TL-1, TL-2).
5. AC-5: A guest session lost during a read is retried once (US19, TL-1, TL-2).
6. AC-6: A remote session resumes after a reset without running a call twice (US20, TL-1, TL-2).
7. AC-7: A repeated execution ID returns the first outcome (US20, TL-1, TL-2).
8. AC-8: A silent connection is detected and resumed (US20, TL-2, TL-3).
9. AC-9: A resume from a new address keeps the session (US20, TL-2, TL-3).
10. AC-10: An expired session leaves outcomes queryable (US20, TL-1, TL-2).
11. AC-11: The remote session passes an HTTPS-only proxy (US20, TL-3).
12. AC-19: A guest session that falls silent fails its request by the silence limit (US19, TL-1, TL-2, TL-4).
13. AC-20: A durable machine is acquired by name (US24, TL-2, TL-6).
14. AC-21: A durable machine admits one lease at a time, across instances (US24, TL-2, TL-6).
15. AC-22: An operation with a stale lease_id is refused (US24, TL-2).
16. AC-23: A command runs as wezzard in the GUI session, without the owner's credentials (US25, TL-2, TL-6).
17. AC-24: Claude Code reaches the gateway with the machine's key (US26, TL-2, TL-6).
18. AC-25: Codex reaches the gateway through a per-session custom provider (US26, TL-2, TL-6).
19. AC-26: A key is never printed, logged or returned (US26, TL-1, TL-2).
20. AC-27: An unsafe or missing key file is reported, not loaded, and never refuses (US26, TL-1, TL-2).
21. AC-28: Push and pull to an instance on another Mac carry the bytes (US27, TL-2, TL-6).
22. AC-29: A machine is chosen by capability (US24, TL-2, TL-6).
23. AC-44: A key without a configured endpoint is not loaded (US26, TL-1, TL-2).

## Orchestrator

[orchestrator.acceptance](orchestrator.acceptance)

1. AC-12: Release cancels a command running over a guest session (US22, TL-2, TL-4).
2. AC-13: Release and destroy leave no guest session behind (US22, TL-2, TL-4).
3. AC-14: A daemon restart makes in-flight calls uncertain, never repeated (US22, TL-2).
4. AC-30: Release sweeps what the lease left running, and nothing else (US28, TL-2, TL-6).
5. AC-31: The inventory before and after is recorded and never refuses (US28, TL-2, TL-6).
6. AC-32: A lease coexists with the CI runner (US29, TL-6).
7. AC-33: A service restart keeps the durable lease (US28, TL-2).

## Operator

[operator.acceptance](operator.acceptance)

1. AC-15: Both channels pass one conformance suite on live VMs (US21, TL-4).
2. AC-16: The guest link is selected by configuration (US21, TL-2).
3. AC-17: Offline suites need no Tart, SSH or network (US21, TL-1, TL-2).
4. AC-18: Live tests leave the installed service alone (US21, TL-4).
5. AC-34: The CLI lists every machine from every instance (US23, TL-2, TL-6).
6. AC-35: Old paths fail loudly after the coordinated switch (US30, TL-2).
7. AC-36: An old variable name stops the daemon, and the installer rewrites it (US30, TL-2).
8. AC-45: An old environment profile is refused with its fix (US30, TL-1, TL-2).
9. AC-37: Each instance is registered in Consul with the tags it warrants (US23, TL-6).
10. AC-38: An instance on a Mac without Tart offers only its durable machine (US31, TL-6).
11. AC-39: Adding and removing an image line changes the list without a restart (US32, TL-1, TL-2, TL-4).
12. AC-40: Editing a line and promoting a base take effect without a restart (US32, TL-2, TL-4).
13. AC-41: A broken line keeps the last good list and is reported loudly (US32, TL-1, TL-2).
14. AC-42: No partial catalog is ever published (US32, TL-1).
15. AC-43: Durable exposure is off unless the operator turns it on (US33, TL-2, TL-6).
16. AC-46: An installation holds exactly the executables the daemon resolves (US34, TL-1).
17. AC-47: A Mac whose Consul node name is not its own name is listed once, by its own name (US23, TL-2, TL-6).
18. AC-48: A Mac with nothing but machctl lists every instance on the network (US23, TL-2, TL-6).
19. AC-49: The consul mode stops the advertisement and asks only Consul (US23, TL-2).

## Relay

[relay.acceptance](relay.acceptance)

1. AC-55: An action runs in the desktop session (US36, TL-7).
2. AC-56: A command behaves as it would in a terminal of the desktop session (US36, TL-7).
3. AC-57: Large output arrives whole (US36, TL-7).
4. AC-58: Files move whole or not at all (US36, TL-7).
5. AC-59: A long action completes (US36, TL-7).
6. AC-60: A lease's relay agent registers and its credential ends at release (US36, TL-7).
7. AC-61: A connection cut during an action does not run it twice (US37, TL-7).
8. AC-62: A silent connection is detected and resumed (US37, TL-7).
9. AC-63: A client that changes its address keeps its session (US37, TL-7).
10. AC-64: A restart of the broker or the relay agent never replays an action (US37, TL-7).
11. AC-65: The relay agent returns after a reboot (US36, TL-7).
12. AC-66: Credentials and ports are guarded (US38, TL-7).
13. AC-67: An expired session still reports its operations (US37, TL-7).
14. AC-68: The client finds the broker by name (US38, TL-7).
15. AC-69: A silent action survives the edge's idle close (US38, TL-7).
16. AC-70: The link is the smallest part of an action's time (US37, TL-7).
