# machine-service (Rust)

This is the Rust translation of the Python `vm-service`, the service's name
before it was renamed machine-service. The Python sources in
`../bin/` remain the behavioral specification and are not replaced by this
workspace until the live acceptance phase completes.

## Layout

- `crates/lease-keys` — per-lease OpenSSH identity creation, bootstrap, and cleanup.
- `crates/environment` — selected-environment profile resolution and startup ownership.
- `crates/acquisition-options` — pure acquisition request validation and reporting.
- `crates/control-only` — the offline control-only contract.
- `crates/application-catalog` — read-only installed-application catalog and base fingerprints.
- `crates/console` — trusted viewer configuration, guest command construction, and the lease-bound console controller.
- `crates/machine-service-core` — lease state, Tart lifecycle, SSH access, and operations.
- `crates/machine-service` — the HTTP daemon.
- `crates/machctl` — the CLI client.
- `crates/console-worker` — the attempt-local console broker.
- `crates/guest-console-agent` — the guest-side sharing adapter.
- `crates/machine-service-install` — launchd installation and removal.
- `crates/network-rig` — the TL-7 network rig: Toxiproxy and mitmproxy with the edge script, the network profiles, the seeded fault schedule and the cell runner. Its self-tests are opt-in: `cargo test -p network-rig --test rig_live -- --ignored`.

Cross-crate interfaces and porting rules are in [`CONVENTIONS.md`](CONVENTIONS.md).
The translation plan is in [`../docs/rust-translation.md`](../docs/rust-translation.md).

## Build and test

```sh
cd rust
cargo build --release
cargo test --workspace
```

The daemon, the CLI, and the console binaries are produced in
`rust/target/release/`. The console subsystem locates its helper binaries at run
time next to the running executable, or through `VM_GUEST_CONSOLE_AGENT` and
`VM_CONSOLE_WORKER`.

## Running the daemon

```sh
rust/target/release/machine-service
```

Configuration comes from the environment, or from a selected profile passed
with `--environment <absolute-path>`.

## Companion binaries

- `machine-service` — the daemon. `machine-service --probe-host` prints the
  GPU and privacy-permission facts as one JSON line; the daemon runs it as its
  own child for the durable machine's capability report.
- `machctl` — the CLI client.
- `console-worker` — launched by the console controller; not run directly.
- `guest-console-agent` — uploaded to the guest by the console controller; not
  run on the host. It is machine code, so one artifact per guest OS is needed:
  the host-native `guest-console-agent` plus `guest-console-agent-<kind>` for
  any other guest OS the service leases. `scripts/build-guest-agents.sh` builds
  both (the Linux artifact is cross-compiled for
  `aarch64-unknown-linux-musl`).
- `machine-service-install` — installs or removes the `com.wezzard.machine-service`
  launchd agent.
