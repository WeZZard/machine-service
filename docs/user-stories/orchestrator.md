# Profile: Orchestrator

A coordinating layer that owns VM lifecycles on behalf of worker agents. Concrete embodiments: the `examples/pi-subagents/vm-use.workflow.js` workflow, a CI job, a supervising daemon that fans tasks out to children.

The orchestrator's defining duty: **teardown happens even when the work fails**. It acquires, delegates, collects evidence, and releases in a `finally` block — and the service's TTL/GC is the backstop if the orchestrator itself dies.

## US8 — Doctrines are enforced mechanically (for our own leases)

**As an** orchestrator, **I want** the host doctrines enforced by the service itself, **so that** no agent (including me) can violate them by accident or by crafting requests.

- **Max 2 concurrent macOS VMs** — the daemon rejects the third acquire (`macOS VM limit reached (2 active)`). Linux is unlimited. This limit is enforced for vm-service's own leases; Apple's underlying limit is host-wide across all Virtualization.framework consumers (UTM, a second Tart, custom tools) and cannot be enforced from here — see US14/US15 for detection and diagnosis, US16 for the shared-resource contract.
- **NAT only** — no bridged networking is ever offered.
- **Lifecycle-managed images are unbootable** — `-base`/`-seed`/`-work` are never started; only purpose clones.
- **Clean clone per task** — every acquire clones; every release destroys.
- The limit counts all `ACTIVE_STATES` (`pending`/`provisioning`/`running`): a pending reservation already holds its slot, because provisioning runs outside the state lock and takes minutes.

**Tests:** `TestConcurrency` (unit: limit, slot freeing, parallel races), `TestAcquireLifecycle` (unit: base never booted), `TestHttpApi.test_macos_limit_409` (integration), `TestDoctrinesE2E.test_macos_limit_enforced_live` (e2e, skips when slots busy).

## US9 — Teardown is guaranteed even if I die

**As an** orchestrator, **I want** leases to expire on their own, **so that** a crashed orchestrator cannot leak VMs and disk (the 2026-08-19 25 GB leftover lesson).

- Every lease carries a TTL. Past TTL the lease is **warned**; past TTL + 6 h grace the VM is stopped, deleted, and unregistered by the GC loop (every 60 s).
- Heartbeats reset both TTL and grace state.

**Tests:** `TestTtlGc` (unit: warn within grace, reclaim after grace, skip non-active records, heartbeat resets).

## US10 — Own the lifecycle end to end

**As an** orchestrator, **I want** a single place to acquire, delegate, collect, and release, **so that** children never manage teardown themselves.

- The workflow acquires, hands the child only `machctl exec/push/pull` verbs, requires evidence pulled to `/var/tmp/` before finishing, and releases in `finally` — even on child failure.
- Child contract: `structuredOutput { verdict, summary, artifacts }`.

**Tests:** E2E via `examples/pi-subagents/vm-use.workflow.js`; `tests/e2e/test_e2e.py::TestLifecycleE2E` proves the real clone→exec→release path against live Tart.

## US22 — Teardown covers sessions too

- As an orchestrator, I want release and destroy to cancel commands running over a guest session and close the session, and a vm-service restart to be reported as uncertain, so that teardown stays guaranteed and nothing is repeated.

**Tests:** AC-12 to AC-14 in [orchestrator.acceptance](../acceptance/orchestrator.acceptance).

## US-N2 — Failure rolls back cleanly

**As an** orchestrator, **I want** a failed provisioning attempt to leave nothing behind, **so that** state and host stay consistent.

- Any failure during clone/configure/boot/SSH-wait/inject stops the VM, deletes it, and drops the lease record. The acquire call errors; no half-provisioned VM survives.

**Tests:** `TestAcquireLifecycle.test_failure_rolls_back_clone_and_state`, `test_ssh_never_ready_rolls_back` (unit).

## US-N3 — Concurrency correctness

**As an** orchestrator, **I want** parallel acquires to be safe, **so that** races never oversubscribe macOS slots or double-lease a purpose.

- State mutations are serialized by a cross-process `flock` plus an in-process mutex; purpose exclusivity and the macOS limit are decided under that lock.
- Verified by racing real threads against the locked phase: 6 threads compete for 2 macOS slots → exactly 2 win; same purpose+image in parallel → exactly one winner, the rest rejected with `already leased`.

**Tests:** `TestConcurrency` (`test_parallel_acquires_never_oversubscribe_macos`, `test_parallel_same_purpose_same_image_one_wins`, `test_parallel_same_purpose_different_images_both_win`) — unit.

## US28 — Every durable lease leaves a hygiene record

- As an orchestrator, I want each durable lease to start in a scratch workspace, to have its leftover processes swept at release, and to keep an inventory before and after, so that I can see what a task left on a machine that outlives it.
- Hygiene reports and never refuses or blocks. See [HY](../arch/hygiene.md).

**Tests:** AC-30, AC-31 and AC-33 in [orchestrator.acceptance](../acceptance/orchestrator.acceptance).

## US29 — Leases coexist with the CI runner

- As an orchestrator, I want leases on 192.0.2.4 and 192.0.2.5 to run beside the readycheck-dev CI runner without pausing it, and I want to know when a CI job ran during my lease, so that I can judge my results.
- See [HY-5](../arch/hygiene.md#hy-5-the-lab-runners-hold-the-lease). The durable Macs are now lab runners whose jobs hold the lease, so a lease and a lab job no longer share the Mac (DC-1xx-LAB-2).

**Tests:** AC-32 in [orchestrator.acceptance](../acceptance/orchestrator.acceptance).
