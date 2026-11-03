# machine-service

Turns this Mac's Tart installation into a **lease-based VM service for agents**.
Agents never touch `tart` directly; they ask the service for a fresh, disposable
VM and give it back when done.

Its virtual machines come from the public
[pilot-images](https://github.com/WeZZard/pilot-images) repository (golden
bases such as `pilot-macos26-base` and `pilot-ubuntu-base`, credential packs).
Clone it and build the bases you need on each Mac that runs virtual machines,
then set `PILOT_REPO` to the checkout when installing. Without `PILOT_REPO`
the instance has no images and checks none (IC-7, DC-94). Enforces the doctrines from pilot-images' CLAUDE.md and the host
AGENTS.md mechanically:

- **Clean clone per task** — every acquire is a fresh COW clone of the golden
  base; every release destroys it (AGENTS.md "MUST create a clean VM instance /
  destroy when done").
- **Max 2 concurrent macOS VMs** (Virtualization.framework limit); Linux unlimited.
- **NAT or internet-only** — no bridged networking is ever offered (MAC-whitelist doctrine). A lease may ask for `network: "internet"`, which runs its guest on Tart's Softnet with no route to the private networks (DC-13), behind a pf anchor that keeps it off the host by its own address (DC-14, DC-15), resolving names over DNS over TLS (DC-16), and with an isolation check before the lease is ready.
- **Lifecycle-managed images are unbootable through the service** — only
  purpose clones are ever started; `-base`/`-seed`/`-work` are never booted by the service. Explicit work-source acceptance reads and clones the configured stopped work image without changing it.
- **Credential env packs are env-var-only** (gateway era) — `env.extra` carries
  LiteLLM gateway envs (pi) and Anthropic-compatible gateway envs (Claude
  Code) plus GH_TOKEN. Gateway keys are not identities: no lane
  balancing, no busy marking; one env pack serves any number of concurrent VMs.
- **Teardown is part of the job** — leases carry a TTL; past TTL the lease is
  warned, past TTL + 6h grace the VM is stopped, deleted, and unregistered.
  This prevents a repeat of the 2026-08-19 style 25 GB leftovers.

## VNC design status

- **The stock-Tart VNC replacement is deployed as part of the Rust port, but its console workflow is not yet live-accepted.** It uses guest-side sharing over SSH; it does not restore the rejected Tart patch.
- [The revised service design](docs/design.md) includes the normal machine-service deployment and mcp-vm-relay upgrade path in the definition of completion. Source tests alone do not establish a working installed workflow.
- [The acquisition contract](docs/acquisition-contract.md) and [console contract](docs/console-contract.md) describe capability discovery, `acquire --vnc`, and explicit `console-resolve`, `console-open`, and `console-cancel` operations.
- [Installation](docs/vnc-installation.md) persists trusted viewer configuration, and [guest preparation](docs/guest-console-provisioning.md) defines the Linux X11 and macOS sharing prerequisites. A configuration check does not prove a stock-Tart boot or a displayed desktop.
- [The mechanism investigation](docs/live-console-review.md) compares stock Tart, Linux X11 sharing, and macOS Screen Sharing, including unresolved compatibility and provisioning checks.
- [The acceptance specification](docs/vnc-acceptance.md) requires actual viewer screenshots, independent visual evaluation, lifecycle tests, and the installed relay workflow.
- [The verification status](docs/capability-transparency-verification.md) distinguishes the new source tests from live acceptance and [historical patched-runtime evidence](docs/history/README.md).

## User stories

Actor-oriented stories (agent / orchestrator / operator), each linked to the
tests that pin it down: [docs/user-stories/index.md](docs/user-stories/index.md).

## Components

The service is a Rust workspace under `rust/`. The Python implementation it
replaced was removed once the port was verified against it, and remains
recoverable from this repository's history.

| Path | Role |
|------|------|
| `rust/crates/machine-service` | Daemon binary: HTTP API on the configured bind address (loopback port 6240 by default), state in `~/.local/state/machine-service/state.json`, GC loop every 60s |
| `rust/crates/machctl` | CLI client (same operations as the HTTP API), and the federated machine list across every instance on the network |
| `rust/crates/discovery` | The discovery mode and the Mac's own DNS-SD service: the daemon's Bonjour advertisement and `machctl`'s browse (DC-84, DC-85) |
| `rust/crates/machine-service-core` | Lease records, Tart lifecycle, SSH access, env packs, and operations |
| `rust/crates/lease-keys` | Standard OpenSSH per-lease key generation, bootstrap, and safe cleanup |
| `rust/crates/acquisition-options` | Pure option validation, configuration reporting, and discovery descriptions |
| `rust/crates/environment` | Selected-environment profile resolution and startup ownership |
| `rust/crates/control-only` | Offline control-only network contract; acquisition refuses while none is available |
| `rust/crates/application-catalog` | Read-only installed-application catalog and base fingerprints |
| `rust/crates/console` | Trusted viewer configuration, lease and attempt identity, guest preparation, and worker supervision |
| `rust/crates/console-worker` | Independently expiring loopback broker, SSH stream, and standard viewer launch |
| `rust/crates/guest-console-agent` | Guest-side sharing agent, built once per guest operating system |
| `rust/crates/runtime-launcher` | Service-owned guest launcher |
| `rust/crates/machine-service-install` | Install/remove the launchd agent `com.wezzard.machine-service` (RunAtLoad + KeepAlive), and print this Mac's service-catalog description |
| `bin/runtime-launcher.c` | C source of the service-owned guest launcher |
| `examples/pi-subagents/vm-use.workflow.js` | Workflow: acquire → child works via machctl → pull evidence → release in `finally` |

## Selected environments

For an isolated Tart store, service state, image checkout, and HTTP endpoint,
select an explicit JSON profile with `VM_ENVIRONMENT_FILE` or
`machctl --environment /absolute/profile.json`. The profile overrides individual
ambient configuration variables. `machctl environment --json` resolves the
configuration offline without creating state directories.

See [Selected environments](docs/selected-environments.md) for the schema,
resolver API, startup ownership, HTTP identity checks, and test scope.
Without a profile, the legacy defaults below remain unchanged.

## Build and install

The daemon is a native executable, so it is launched directly instead of
through an interpreter. It finds its helpers beside its own executable, and
the installer installs exactly those executables: both read one list, the
`service-layout` crate ([IN](docs/arch/installation.md), DC-79).

```zsh
# 1. Build every executable of the installation, each under its installed
#    name, into target/machine-service/ (the Linux guest agents need
#    `rustup target add aarch64-unknown-linux-musl` and aarch64-linux-musl-gcc).
cd vm-service/rust   # your checkout
scripts/build-release.sh

# 2. Validate, then install into the install folder. Loopback only is the
#    fresh-install default.
target/machine-service/machine-service-install install --check --prefix ~/.local/libexec/machine-service
target/machine-service/machine-service-install install --prefix ~/.local/libexec/machine-service

# With images: name the pilot-images checkout (none is assumed; DC-94).
PILOT_REPO=/path/to/pilot-images target/machine-service/machine-service-install install --prefix ~/.local/libexec/machine-service

# LAN-reachable (binds 0.0.0.0; required for consumers on other hosts, and
# for the Bonjour advertisement). A repeated install preserves the bind address it finds.
MACHINE_SERVICE_HOST=0.0.0.0 target/machine-service/machine-service-install install --prefix ~/.local/libexec/machine-service

# Expose this Mac as a durable machine (off by default; kept by later installs).
target/machine-service/machine-service-install install --prefix ~/.local/libexec/machine-service --expose-durable on

launchctl list | grep machine-service     # health check
curl -s localhost:6240/health
```

- With Tart, the installation is `machine-service`, `machctl`,
  `machine-service-install`, `console-worker`, `guest-console-agent` and
  `guest-console-agent-linux`, `guest-session-agent` and
  `guest-session-agent-linux`, `vm-service-dot-forwarder` and
  `vm-service-guest-firewall`. A guest agent runs inside the guest as machine
  code, so a Linux guest needs the `-linux` build.
- The macOS relay agent of a release is signed with Developer ID; how the
  owner signs a release and how a durable Mac's operator grants it its
  permissions once is in [signing](docs/signing.md).
- `install --prefix DIR` checks that every one is beside the installer and
  refuses by name, before anything changes, when one is missing. After the old
  job has stopped it copies exactly those files into `DIR`, and the LaunchAgent
  and the `machctl` link point there. Without `--prefix`, the folder beside the
  installer is the install folder.
- The installer refuses to install, restart, or remove while any lease record
  remains in the previous or the proposed state root, including retained failed
  or deleting records. Release the leases first.
- The installer preserves installed environment values such as `MACHINE_SERVICE_HOST`,
  `MACHINE_SERVICE_CONSOLE_CONFIG`, and the selected-environment selector, unless the
  caller supplies an override for that setting.
- Enabling the console for the first time requires an explicit
  `--console-config PATH` argument.
- `--check` and `--dry-run` validate without writing a plist or calling
  `launchctl`.
- `rust/scripts/build-guest-agents.sh` builds only the guest agents, in the
  release profile, into `target/guest-agents`; `build-release.sh` uses it.
- `machine-service-install install` links `~/.local/bin/machctl` to the
  installed `machctl`. An existing link is replaced, and a file that is not a
  link is left in place.
- `machine-service-install remove` unloads the agent, deletes the plist, and
  removes the `machctl` link it made.
- The daemon refuses to start while any `VM_SERVICE_*` variable is set, and
  names each one with its `MACHINE_SERVICE_*` replacement. The installer
  refuses a caller that still exports one.

### A Mac without Tart (a durable machine only)

A Mac with no Tart and no images creates no virtual machine, so its instance
offers only its durable machine (MG-4 step 5, AC-38, DC-65, DC-66). It needs
three executables. The guest agents, the console worker, the guest firewall
and the DNS-over-TLS forwarder serve virtual machines only, and the installer
does not require them there.

| Path on the Mac | What it is |
| --- | --- |
| `~/.local/libexec/machine-service/machine-service` | the daemon the LaunchAgent runs |
| `~/.local/libexec/machine-service/machctl` | the CLI, linked from `~/.local/bin/machctl` |
| `~/.local/libexec/machine-service/machine-service-install` | the installer |
| `~/Library/LaunchAgents/com.wezzard.machine-service.plist` | the LaunchAgent the installer writes |
| `~/.local/state/machine-service/` | state, `service.log`, `durable.json`, `leases/` |
| `~/.config/machine-service/agents/` | the agent keys and `endpoints.json`, written by the owner |
| `~/machine-service/leases/<lease_id>/` | each durable lease's workspace |
| `/var/tmp/com.wezzard.machine-service/durable-machine.lock` | the host-wide durable lease lock |

On the build Mac, build the three release executables and pack them:

```zsh
cd vm-service/rust   # your checkout
cargo build --release -p machine-service -p machctl -p machine-service-install
tar -C target/release -czf /tmp/machine-service-durable.tgz \
  machine-service machctl machine-service-install
```

Copy `/tmp/machine-service-durable.tgz` to the Mac without Tart. Then, there,
as the account that runs the service, logged in to its GUI session:

```zsh
# 1. Stage the executables where the installer expects its siblings.
install -d ~/.local/libexec/machine-service
tar -xzf ~/machine-service-durable.tgz -C ~/.local/libexec/machine-service
chmod 0755 ~/.local/libexec/machine-service/{machine-service,machctl,machine-service-install}
xattr -d com.apple.quarantine ~/.local/libexec/machine-service/* 2>/dev/null || true

# 2. Validate, then install: bound to all interfaces, with the Mac exposed.
MACHINE_SERVICE_HOST=0.0.0.0 ~/.local/libexec/machine-service/machine-service-install install --check --expose-durable on
MACHINE_SERVICE_HOST=0.0.0.0 ~/.local/libexec/machine-service/machine-service-install install --expose-durable on

# 3. Check: the instance offers its durable machine and nothing else.
curl -s localhost:6240/health       # "kinds": ["durable"]
~/.local/bin/machctl list --local   # one durable row, state available

# 4. Describe it to the service-catalog registrar (tagged durable, not vm).
~/.local/libexec/machine-service/machine-service-install catalog-description \
  > ~/.config/service-catalog/services/machine-service.json
```

- The installer says `No Tart found: this instance offers only its durable
  machine` and links `~/.local/bin/machctl`. Without Tart and with exposure
  off, it installs an instance that offers nothing and says so ([A Mac that
  offers nothing](#a-mac-that-offers-nothing)).
- `GET /health` answers `kinds: ["durable"]`, `GET /machines` lists the
  durable machine and no offer, and `POST /acquire` with an image answers 400
  `this instance cannot create virtual machines: Tart is not installed`.
- Leave `PILOT_REPO` unset: the instance then has no image source, checks no
  image files and warns about none (IC-7).
- The executables are built for macOS 11 and later (Cargo's default), so a
  build on a newer macOS runs on macOS 26. Cargo signs them ad hoc, so macOS
  forgets the privacy grants of `machine-service` whenever a new build is
  copied in: grant Screen Recording, Accessibility and input to
  `~/.local/libexec/machine-service/machine-service` again in System Settings
  after each deployment, until it is signed with a stable identity (DM-4).
- The owner writes the agent keys and `endpoints.json` ([Agent
  keys](#agent-keys)). A reinstall restarts the daemon, which keeps a
  current durable lease and its processes (DM-8).

### A Mac that offers nothing

A Mac that only runs `machctl`, such as a laptop, can be configured to provide
neither its durable machine nor virtual machines: install without Tart (or
without `PILOT_REPO`) and with durable exposure off. Its own configuration then
says it provides nothing, and no list of names is needed (DC-93).

```zsh
~/.local/libexec/machine-service/machine-service-install install --expose-durable off
~/.local/bin/machctl list --local   # one row: none  provides no machines
```

- The installer says `this instance offers nothing`. The default loopback bind
  keeps it off the LAN and out of Bonjour, so other Macs do not list it.
- Without any service on the Mac, `machctl list` shows the local row as `not
  running`, not `unreachable`.

### Migrating from vm-service

The service was renamed from vm-service, and its CLI from `vmctl` (DC-19,
DC-35). Release every lease first; then `machine-service-install install`
migrates an installed `com.wezzard.vm-service` agent in the same run:

- The old agent's `VM_SERVICE_*` values are written under their
  `MACHINE_SERVICE_*` names into `com.wezzard.machine-service`.
- The old agent's job is unloaded and its plist removed.
- `~/.local/state/vm-service/` moves to `~/.local/state/machine-service/` when
  the new folder does not exist yet. It holds no lease and no lease key,
  because the installer refuses while any lease record exists; the service log
  moves with it.
- `~/.config/vm-service/console.json` is copied to
  `~/.config/machine-service/console.json` when the new file does not exist,
  and the console variable is pointed at the copy.
- The `~/.local/bin/vmctl` link into the old install folder is removed, and no
  `vmctl` link is installed.
- The old install folder `~/.local/libexec/vm-service/` is left in place for
  the operator to delete.

`--check` prints this plan without changing anything.

Log at `/tmp/com.wezzard.machine-service.{out,err}`, service log at
`~/.local/state/machine-service/service.log`.

## Guest firewall for `internet` leases

An `internet` lease needs the guest firewall, a pf anchor whose table lists
the running `internet` guests by address and keeps them off this host
(DC-14, DC-15, [acquisition contract](docs/acquisition-contract.md#guest-network)).
Install it once, as root, from the account that runs machine-service:

```zsh
sudo ~/.local/libexec/machine-service/vm-service-guest-firewall install   # --table PATH for another table file
~/.local/libexec/machine-service/vm-service-guest-firewall status         # no root needed; lists the table's guests
sudo ~/.local/libexec/machine-service/vm-service-guest-firewall remove
```

- `install` creates the table file `/var/db/com.wezzard.vm-service/internet-guests`, owned by the account that ran `sudo`, writes `/etc/pf.anchors/com.wezzard.vm-service` and the LaunchDaemon `com.wezzard.vm-service.guest-firewall`, enables pf, and loads the anchor into `com.apple/250.vm-service`. `print` shows the anchor and the LaunchDaemon without installing them.
- Every machine-service instance on the host writes the same table file. `MACHINE_SERVICE_GUEST_FIREWALL_TABLE` points an instance at another one, which the anchor must read.
- Install the firewall while no `internet` lease is running, because SSH connections opened before the anchor loads have no pf state.
- `MACHINE_SERVICE_GUEST_DNS` sets the resolvers an `internet` guest reaches over DNS over TLS, by default `1.1.1.1#cloudflare-dns.com,8.8.8.8#dns.google`.
- A macOS `internet` guest needs `vm-service-dot-forwarder` beside machine-service, or `MACHINE_SERVICE_DOT_FORWARDER`; the host-native build runs in a macOS guest.
- `MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR` points the service at another anchor file, as the test fixtures do.
- `--expose-durable on|off` writes `MACHINE_SERVICE_EXPOSE_DURABLE` into the
  LaunchAgent, and later installs preserve it. Exposure is off unless the
  operator turns it on (DC-39).

## Discovery

Every instance bound to all interfaces advertises itself with Bonjour, the
network's own discovery: `_machine-service._tcp`, named by its instance ID, with
`txtvers=1`, `instance` and `kinds` in its TXT record. `machctl` and other
clients browse for it, so a Mac finds every instance with nothing else
installed, and confirm each one by its `/health` before using it.
`MACHINE_SERVICE_DISCOVERY` chooses the mode for the daemon and every client:
`bonjour` (the default) or `consul`. With `consul`, the daemon advertises
nothing and clients ask only the Consul agent, as described below (FD-1,
FD-2, DC-84, DC-85). Any other value stops the daemon at startup.

```bash
dns-sd -B _machine-service._tcp local.   # the instances advertising on this network
```

### Consul mode

Each Mac's service-catalog registrar publishes its instance in Consul from a
description file in the registrar's own folder. The installer prints that file
for the Mac it runs on; the operator places it:

```zsh
machine-service-install catalog-description > \
  ~/.config/service-catalog/services/machine-service.json
```

- The tags say what the instance offers: `machines` and `agents` always,
  `durable` when durable exposure is on, and `vm`, `tart` and
  `disposable-infra` when the instance can create virtual machines (Tart is
  installed, the image catalog builds, and a line's base VM is in the Tart
  store).
- Exposure is `--expose-durable on|off` when given, else what the installed
  LaunchAgent sets. The command explains each tag on stderr, warns when the
  instance binds loopback, and never writes the file itself.
- `services/machine-service.json` is its output for a Mac with Tart and images
  and exposure off.
- The registrar keeps service `machine-service` in Consul with the file's tags,
  this Mac's LAN address and port 6240 (`loopback_only: false` when the
  instance binds `0.0.0.0`), and an HTTP check on `GET /health` every 30 s.
- The file also carries `"meta": {"instance": "<LocalHostName>"}`, which the
  registrar publishes as the registration's service metadata. The Consul node
  name comes from the Consul agent's configuration and need not be the Mac's
  name (192.0.2.3 is node `mac-a-m4` and `Mac-A`), so this is how
  a client knows which instance a registration is. It needs a registrar that
  publishes a description's `meta`.
- `machctl list` reads the registrations from the local agent
  (`GET /v1/health/service/machine-service`) and matches each to an instance
  by its `instance` metadata and port. A registration without it is named by
  its node name, and `machctl` says so on stderr; see [CLI](#cli).

The API itself has no authentication — LAN exposure assumes the household
trust model (same as the LLM gateway). Keep the loopback-only bind where that
model does not hold.

## HTTP API (agents)

```
GET  /health
GET  /acquisition-capabilities
GET  /images
POST /images/reload
GET  /applications
GET  /machines
GET  /machines/<name>
POST /acquire                {"purpose":"my-task","image":"macos26","env":"default","ttl_hours":4}
POST /machines/<name>/exec        {"argv":["uname","-a"]}  or  {"script":"echo hi"}
POST /machines/<name>/push        {"local_path":"/host/f","remote_path":"/tmp/f"}
POST /machines/<name>/pull        {"remote_path":"/tmp/f","local_path":"/host/f"}
PUT  /machines/<name>/files?lease_id=<id>&path=<remote path>[&name=<item>]   body: bytes or tar
GET  /machines/<name>/files?lease_id=<id>&path=<remote path>
POST /machines/<name>/heartbeat   {"ttl_hours":4}
POST /machines/<name>/release     {"reason":"done"}
POST /gc

# Durable machine (exposure on only); every operation carries "lease_id".
POST /acquire                     {"machine":"<instance>","purpose":"metal-check","env":"default","ttl_hours":4}
POST /machines/<instance>/exec    {"lease_id":"...","argv":["uname","-a"],"cwd":"sub","timeout":600}
POST /machines/<instance>/push    {"lease_id":"...","local_path":"/host/f","remote_path":"f"}
POST /machines/<instance>/pull    {"lease_id":"...","remote_path":"f","local_path":"/host/f"}
POST /machines/<instance>/heartbeat {"lease_id":"...","ttl_hours":4}
POST /machines/<instance>/release {"lease_id":"...","reason":"done","keep_workspace":false}
POST /machines/<instance>/refresh
GET  /machines/<instance>/leases/<lease_id>/hygiene
```

Errors are JSON `{"error": "..."}` with status 400/404/409/500.

- `GET /machines` lists the leases under its top-level `machines` key. Each
  lease record keeps its fields, including `vm`, and carries `kind: "virtual"`,
  `instance` and its image's `capabilities`. With durable exposure on, the
  instance's own Mac is listed there too, under its instance ID
  ([Durable machine](#durable-machine)).
- `GET /machines` also lists `offers`: one per image of the current catalog,
  with `name` and `image` (the image), `kind: "virtual"`, `state: "offer"`,
  `image_kind`, `base_vm`, `base_available`, `defaults`, `concurrency`
  (`running`, `limit`, `acquirable`, as `GET /images` has them) and
  `capabilities` read from the image: the line's resource defaults, the
  measured paravirtual GPU of a macOS guest (`mac2`, `common3`, no `metal3`),
  and the software of the image's validated application inventory. Facts only
  a running guest could answer are `null` with their reason under `unknown`.
  A Mac without Tart, or without a catalog, lists `offers: []` and says why
  in `offers_unavailable`.
- `PUT` and `GET /machines/<name>/files` move bytes for a client on another
  Mac, for both kinds of machine ([Byte transfers](#byte-transfers)).
- `GET /health` answers `ok`, `service`, `instance` (the Mac's
  `LocalHostName`) and `kinds`: `durable` when exposure is on, and `virtual`
  when the image catalog is available and a Tart executable is found (`TART`
  or `tart` on `PATH`). An image acquisition on a Mac without Tart answers 400
  `this instance cannot create virtual machines: Tart is not installed (no
  tart on PATH)`.
- `GET /images`, `GET /machines` and `GET /health` carry the image catalog's
  status under `catalog`, and `POST /images/reload` rebuilds the catalog and
  answers with it. See [Image catalog](#image-catalog).
- Every `/vms` and `/vms/<name>/...` path was removed. It answers HTTP 410 Gone
  with an error naming the `/machines` path that replaces it, and a
  `replacement` field carrying that path. No operation runs (DC-37).

## Image catalog

The daemon keeps one immutable snapshot of its image lines, `images/*/line.conf`
in `PILOT_REPO`, and every reader of the lines uses it: `/images`, `/machines`,
`/applications`, `/acquisition-capabilities` and `POST /acquire`. Adding,
editing or removing a line takes effect on the next request, without a restart
([IC](docs/arch/image-catalog.md), DC-34).

- **Trust.** Only the daemon that owns `daemon.lock` **executes** `line.conf`
  files, with the strict zsh parser and its 15-second timeout. It builds the
  first snapshot after taking the lock and before it serves; nothing is
  executed for a request before then.
- **Input identity.** Each request that reads the catalog first lists
  `images/` and, for each folder with a `line.conf`, takes the file's inode,
  size, modification time and SHA-256. This executes nothing. When the
  identity differs from the snapshot's, the request asks for a rebuild and
  waits for it, up to 30 seconds. One rebuild runs at a time, and requests
  that arrive meanwhile wait for that same rebuild.
- **Strict build.** A rebuild fails as a whole on an unreadable `images/`
  folder, no lines, more than 64 lines, a non-regular `line.conf`, a parse or
  source failure or timeout, an invalid image or base name, a kind other than
  `macos` or `linux`, or a `CPU` or `MEMORY_MB` that is not an integer. There
  is no permissive mode and no built-in fallback list. The builder checks the
  identity again after executing the files and starts over if they moved.
- **Atomic swap.** A complete snapshot replaces the old one in one pointer
  swap. A request, and an acquisition from start to end, keeps the snapshot it
  read. Guest passwords stay beside the snapshot in the daemon's memory for
  the lease bootstrap, never in a response or the log.
- **Failure.** A failed rebuild keeps the last good snapshot and writes one
  `ERROR` line to the service log and to stderr per failed input identity.
  Requests do not retry a failed identity; the next change to the files, or
  `POST /images/reload`, does. Without any good snapshot, as after a startup
  with broken files, `/images` and `/applications` answer HTTP 503 with the
  diagnostic and `POST /acquire` answers 409; a selected environment refuses
  to start instead.
- **Status.** `catalog` is `{"generation", "built_at", "status"}`, where
  `status` is `current`, `stale` (the files changed and the snapshot no longer
  matches them) or `unavailable` (no good snapshot yet). After a failed
  rebuild that is not superseded, it adds `error`, `failed_at` and `file`.
  `/health` reports it without waiting for a rebuild.
- **Live inputs.** Whether a base VM exists in the Tart store, and so whether
  an image is acquirable, is read on every request, so promoting a base needs
  no rebuild. Inventories, associations and fingerprints stay live too.
- **Watcher.** A kqueue watcher on `images/`, each line folder and each
  `line.conf` starts a rebuild as soon as a file changes, so the next request
  usually finds the catalog current. It only accelerates: a missed event costs
  one request's wait, never a stale answer.
- **Removed lines.** A lease whose line is removed keeps running; its
  heartbeat, exec and release read the lease record, not the catalog.

## Durable machine

The Mac an instance runs on can be leased as a whole: an exclusive session as
the service's own account, with a scratch workspace. The design is in
[durable machine leases](docs/arch/durable-machine.md),
[agent keys](docs/arch/agent-keys.md) and [hygiene](docs/arch/hygiene.md).

- **Exposure is off unless turned on.** `MACHINE_SERVICE_EXPOSE_DURABLE=on` in
  the instance's environment exposes the Mac; `machine-service-install
  --expose-durable on|off` writes it, and a later installation without the
  flag keeps it. With exposure off, `/machines` lists no durable machine,
  `/health` reports no `durable` kind, and every durable request answers 404
  `durable machine not exposed on <instance>` before any lock or workspace is
  touched. Any value but `on` or `off` stops the daemon at startup.
- **Name.** The durable machine is named by the instance ID, the Mac's
  `scutil --get LocalHostName`.
- **Acquire.** `POST /acquire` with `machine` takes `purpose`, `ttl_hours`
  (default 24, at most 720) and `env` (`default` loads the agent keys, `none`
  loads none). `machine` with `image` is refused with 400, as are `cpu`,
  `memory_mb`, `disk_gb`, `vnc`, `network`, `source` and
  `expected_source_fingerprint`. The answer is the machine record with
  `kind: "durable"`, the `lease_id`, the `workspace` and the `capabilities`.
- **One lease per Mac.** The lock file
  `/var/tmp/com.wezzard.machine-service/durable-machine.lock` names its
  holder's state folder, lease, purpose and expiry, and every instance on the
  Mac shares it. A second acquisition answers 409 `durable machine leased:
  <name> (purpose <purpose>, until <time>)`. A holder past its TTL, grace and
  one more hour may be taken over.
- **`lease_id` on every operation.** A missing or stale `lease_id` answers 409
  and nothing runs.
- **Workspace.** `~/machine-service/leases/<lease_id>/` holds `work/` (the
  default working directory), `tmp/` (`TMPDIR`) and `codex/` (`CODEX_HOME`),
  all mode 700. `HOME` stays the account's home.
- **Exec.** A command runs as a child of the service, in a new process
  session, with the session environment: the login shell's (`zsh -l`)
  variables without `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
  `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `OPENAI_API_KEY`,
  `OPENAI_BASE_URL`, `LITELLM_CODEX_KEY`, `CODEX_HOME` and the service's own
  `MACHINE_SERVICE_*` settings, plus `MACHINE_SERVICE_LEASE_ID`,
  `MACHINE_SERVICE_WORKSPACE`, `TMPDIR`, `CODEX_HOME` and the loaded agents'
  variables. `argv` runs through `$SHELL -c` with the arguments quoted;
  `script` is fed to `zsh -s`. A relative `cwd` resolves against `work/`. The
  answer is `{name, vm, rc, output}` with every loaded key's value replaced by
  `[redacted]`. A timeout or a release kills the command's whole session. The
  answer returns when the shell exits, even when a background process keeps
  its output open.
- **Push and pull** copy between two paths on the Mac for a loopback client
  only, through a temporary sibling and a rename, and report `bytes` and
  `sha256`. A relative `remote_path` resolves against `work/`. Paths in the
  agent key folder or the lease SSH material are refused. A client on another
  Mac uses the byte-carrying endpoints ([Byte transfers](#byte-transfers)).
- **Unavailable.** Without a lease, the machine is `unavailable`, with
  `unavailable_reason`, while the service's user has no logged-in GUI session
  (the console belongs to another user or to the login window). An
  acquisition then answers 409 `durable machine unavailable: <name>
  (<reason>)`, a capacity refusal a client may wait on. A display that could
  not be read never makes the machine unavailable.
- **Warnings.** The machine record's `warnings` lists `{code, message}`
  entries. Below `MACHINE_SERVICE_DISK_WARN_GB` free GiB (default 30; `0`
  turns it off; any value that is not a number stops the daemon) it carries
  `low-free-disk`, `low free disk: <n> GiB free, below the warning level of
  <n> GiB`. A warning never makes the machine unavailable.
- **Heartbeat, TTL and GC.** A heartbeat renews the TTL (its `ttl_hours`, or
  the lease's own) and the lock's expiry. Past its TTL a lease is warned; past
  the 6-hour grace GC releases it.
- **Release** stops every process in the lease's recorded sessions
  (`SIGTERM`, 10 s, `SIGKILL`), sweeps the leftovers, takes the inventory
  after, removes the workspace unless `keep_workspace` is set, and frees the
  lock. It answers `{name, lease_id, released, reason, hygiene}`. The lease's
  record stays for 30 days, and a kept workspace for 7.
- **Restart.** A restart of the service keeps the lease, its lock and its
  processes, which run in their own sessions. The session environment and the
  keys are rebuilt on the first operation after it. An acquisition that a
  restart interrupted is rolled back, and an interrupted release is finished.
  A lease found on an instance whose exposure is now off is released.

### Agent keys

- `~/.config/machine-service/agents/` (`MACHINE_SERVICE_CONFIG_DIR` names
  another configuration folder) holds `claude-code.env`
  (`ANTHROPIC_API_KEY=<key>`), `codex.env` (`LITELLM_CODEX_KEY=<key>`) and
  `endpoints.json`
  (`{"claude-code": {"base_url": "http://<gateway-host>"}, "codex": {"base_url": "http://<gateway-host>/v1"}}`).
  The owner writes them; the service never creates, rotates or copies a key,
  and has no default endpoint.
- The folder must be mode 700 and the key files mode 600, owned by the
  service's user; `endpoints.json` must not be writable by group or other.
  A key file holds exactly one assignment, plus blank and `#` lines.
- An agent is loaded only when its key and its endpoint are both valid.
  Claude Code then gets `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`; Codex
  gets `LITELLM_CODEX_KEY` and a `codex/config.toml` that selects the
  `machine-service-litellm` provider at the configured base URL with
  `env_key = "LITELLM_CODEX_KEY"` and no model.
- The machine's `agents` capability reports each key as `present`, `absent`,
  `unsafe-mode` or `invalid` and each endpoint as `present`, `absent` or
  `invalid`; none of them refuses an acquisition. No key value reaches a
  record, a log, a response or a file the service writes.

### Capabilities

`capabilities` (schema version 1) carries `os`, `hardware` (chip, cores,
memory, hardware UUID), `disk`, `gpu` (name, Metal families, `metal3`,
argument buffer tier), `display` (console user, GUI session, whether the
screen is locked, from `ioreg`'s `IOConsoleUsers`), `software`
(node, cua-driver, Claude Code, Codex, Tart, Xcode, Unity, Unreal),
`permissions` (Screen Recording, Accessibility, event posting), `agents`,
`ci_runner`, `network: "lan"`, `observed_at` and `unknown`. A fact the
instance could not observe is `null` with its reason under `unknown`. Disk,
display, CI runner and agents are read on every request; the rest at
startup, daily, at the start and end of each lease and on `refresh`. The GPU
and permission facts come from `machine-service --probe-host`, which the
service runs as its own child so macOS attributes the checks to it.

### Hygiene

- A process is swept at release when it runs as the service's user, started
  after the lease began, and belongs to a session the lease started, descends
  from one, or started with this lease's `MACHINE_SERVICE_LEASE_ID`. One
  process snapshot is classified before any signal, so a child that left the
  session while its parent ran is still found.
- A regular application (one with a Dock presence) is swept when the lease
  launched it: it started after the lease began, was not running in the
  inventory before, and its launchd job (`application.<bundle>…` in the
  user's GUI domain) was started with this lease's `MACHINE_SERVICE_LEASE_ID`,
  which `open` hands to the job from the lease command's environment. An
  application that was already running, or that the owner or another program
  opened, is never touched; `open -a` of a running application only
  activates it. Only that one variable is parsed from `launchctl print`, and
  nothing of its output is logged or kept. A matched application is asked to
  quit (`NSRunningApplication.terminate`, through `osascript`), given 10 s,
  then sent `SIGTERM` and `SIGKILL`; the hygiene record lists it with rule
  `application` and signal `QUIT`, `TERM` or `KILL`. An application that
  CuaDriver's own `launch_app` starts carries CuaDriver's environment and is
  not swept.
- The sweep never ends the service or its ancestors, the CI runner
  (`Runner.Listener`, `Runner.Worker`, `Runner.PluginHost`, or anything under
  `~/actions-runner`) or anything descending from it, or a process under
  `/System/Library/CoreServices/`. A spared match is listed with its reason.
- macOS 27 does not let one process read another's environment, so the
  environment rule finds nothing there: a process that left the lease's
  sessions after its parents exited survives the release.
- The inventory before and after records the user's processes (CI-runner
  processes marked `origin: ci`), the running regular applications, the
  installed applications, the launchd jobs, the Homebrew formulae and casks
  (two parts), the global npm packages (`npm_globals`, `{name: version}`),
  the crontab, the top-level entries of the home folder, Application Support
  and Preferences, the free disk and the `software` facts. Each part has 60 s;
  a part that fails is recorded under `missing` with its reason.
- A crontab entry is recorded as its schedule and its program, keyed by the
  SHA-256 of the whole line, so a change shows and no argument (a token
  written into a crontab line) reaches a record that any LAN client can read.
- Whether a CI job runs is sampled at acquisition, on every GC pass (once a
  minute, and on `POST /gc`) and at release. The hygiene record's
  `ci_busy_intervals` lists `{from, to}` for each stretch a job was seen
  running, an interval still open at release closing there, and
  `ci_sampling` says how often it looked (`interval_s`, `samples`). A job
  that starts and ends between two samples is not seen.

## Application catalog

`GET /applications` returns the complete validated installed-application catalog
from Git-visible `PILOT_REPO/images/<image>/applications.json` portable observations
paired with host-local associations, or HTTP 503 when unavailable or invalid.

- The state root is `PILOT_IMAGES_STATE_DIR` when it is nonempty, otherwise
  `$XDG_STATE_HOME/pilot-images` when `XDG_STATE_HOME` is nonempty, otherwise
  `~/.local/state/pilot-images`.
- Each association is at `<state-root>/stores/<store-id>/base/<image>.json`.
- The store ID is the full lowercase SHA256 hexadecimal digest of the UTF-8
  canonical absolute Tart VM-store root, without a trailing slash. It identifies
  the store directory, not an individual VM or the pilot-images checkout.
- The daemon uses `$TART_HOME/vms` when TART_HOME is set, or `~/.tart/vms` otherwise. The catalog loader also accepts an explicit `base_root` for fixtures and embedded callers.
- Missing members are reported by exact path in diagnostics. If no inventory is
  usable, the HTTP 503 response also includes those paths.

Portable schema 1 is closed `{schemaVersion,image,
inventory,provenance}`; association schema 2 is closed `{schemaVersion,image,base,
inventorySha256}` and hashes complete portable bytes. Provenance records bounded
extraction mode/evidence identifier and lowercase raw/collector/aliases SHA256.
Missing pairs, digest mismatches and stale bases exclude their inventories;
malformed existing members fail the request even if the mate is missing. Both
members and fingerprint are rechecked against ordinary concurrent replacement.
No legacy migration/rebind. Historical Git-visible initial observations alone
are review-ready, not current-base validity or installation certification.

Requests perform **metadata-only** inventory/fingerprint reads, not shell/config
execution, lifecycle operations or inventory generation. Portable inventory
JSON, host-local association JSON, and base fingerprints remain live on every
request. The image-to-`{kind, base_vm}` associations come from the image
catalog above, and the response keeps its closed shape, which mcp-relay
validates: it carries no catalog status. Without any good catalog it answers HTTP 503 with the diagnostic.

### Migration and rollback

- Coordinate the producer and consumer source versions before an operator-approved
  deployment. The consumer has no fallback to `lines/`, `inventories/`, or
  `inventories/local/base/`.
- Move image configuration to `images/<image>/line.conf` and portable observations
  to `images/<image>/applications.json`. Preserve portable bytes exactly because
  the association digest covers the entire file.
- Configure the producer and daemon with the same state-root environment and Tart
  VM-store root. Host-local records are not portable between stores or hosts.
- Use the producer's explicit validation/publication workflow to establish current
  associations. Do not generate or rebind records through catalog search, and do
  not move or modify golden VMs to make an old fingerprint pass.
- Preserve the old checkout and its local association records before migration.
  After an explicitly approved deployment, verify `GET /applications`; the
  daemon reads changed image lines on the next request, without a restart.
- To roll back, restore the matching old producer and consumer versions together
  with the old configuration and inventory layout. Restore only association
  records that still match the same local base fingerprints. A stale record must
  remain unavailable rather than being silently rebound.
- Source changes and fixture tests do not deploy or restart the shared daemon.

### Execution timeout

- `POST /machines/<name>/exec` accepts a positive `timeout` in seconds and defaults to
  600 seconds. The daemon forwards the requested value to the SSH subprocess
  without imposing a shorter deadline.
- A receiver running a command for up to 3600 seconds must include its capture
  and reporting allowance in this timeout. For example, a 120-second allowance
  requires a 3720-second request timeout. Longer explicit requests remain valid.
- The HTTP caller must set a transport timeout longer than the complete receiver
  budget and must continue heartbeating the lease during long operations.
- A release of the lease preempts a running command, push, or pull. The daemon
  kills the SSH or SCP subprocess, the request fails with
  `command cancelled by release of <name>` or
  `transfer cancelled by release of <name>`, and teardown proceeds. See
  [Lease lifecycle fixes](docs/lifecycle-fixes.md).

### Fresh-work-clone acceptance

- Acquisition defaults to `source: "base"`. An image maintainer can explicitly request `source: "work"` to test a fresh clone of that image's configured `WORK_VM` before promotion.
- Work-source acquisition requires explicit `env: "none"`. It does not accept an arbitrary VM name or path, and it never boots or modifies the source work image.
- The work image must exist and be stopped. The daemon checks its metadata before reservation and immediately before and after cloning. A changed source fails acquisition and cleans up only the new lease.
- An optional `expected_source_fingerprint` object requires the source metadata to match the maintainer's recorded work association before cloning.
- The returned lease includes `source`, `source_vm`, and `source_fingerprint`. The maintainer must also recheck the source after acceptance and bind the actual clone report to that work association before promotion. File metadata checks are not disk-content attestation.
- The image maintenance lock must remain held by the maintainer throughout work acceptance and promotion. This is not authorization to run builds or restart the production daemon automatically.

```sh
machctl acquire --purpose acceptance-example --image ubuntu2404 --source work --env none \
  --expected-source-fingerprint /path/to/work-fingerprint.json
```

## CLI

```zsh
machctl images                    # images, capacity and the catalog's status
machctl images --reload           # rebuild the catalog now; exit 1 unless current
machctl acquire --purpose build-test --image macos26 --ttl-hours 4
machctl acquire --purpose bench --require metal3 --require unreal   # any matching machine
machctl acquire --purpose check --machine mac-b                     # a named durable machine
machctl acquire --purpose x --image ubuntu2404 --instance mac-a
machctl list                      # every machine from every instance
machctl list --local              # only this Mac's instance; nothing is browsed or asked
machctl list --kind durable --require metal3
machctl exec pilot-mac-build-test-<id> -- swift --version
machctl status <vm>@<instance>    # when two instances have a machine of that name
machctl push  <vm> ./artifact.zip /tmp/artifact.zip
machctl pull  <vm> /tmp/build.log ./build.log
machctl push  --bytes <vm> ./folder /tmp/   # send the bytes even on loopback
machctl exec  <machine> --lease-id <id> -- uname -a   # override the remembered lease
machctl heartbeat <vm>            # call periodically for long tasks
machctl release <vm>
```

`machctl list` asks the local instance, then browses `_machine-service._tcp`
for 2 seconds, or in Consul mode asks the local Consul agent
(`CONSUL_HTTP_ADDR`, default `http://127.0.0.1:8500`) for every instance
registered as `machine-service`. It confirms each instance it found by its
`/health`, asks every instance at once, and prints one
table: grouped by instance, local first, the durable machine, then virtual
machines, then offers. Each instance gets one 5-second budget, so a dead
instance never hangs the list.

- An instance that does not answer keeps one row with its error and, in Consul
  mode, its Consul health, for example
  `unreachable  connection refused (Consul: critical)`. An instance whose
  `/health` answers another ID than it was found under is listed unreachable
  with both IDs. A
  registration published loopback-only on another Mac is not asked.
- A Consul agent that does not answer, or a browse that fails or finds no
  other instance, is named on stderr with how many instances answered, and
  the local instance is still listed.
- `--json` prints one object per instance: `instance`, `address`, `local`,
  `reachable`, `error`, `found_by` (`local`, `bonjour`, `consul` or `address`),
  `consul`, `catalog` and `machines`.
- `--instance <ID or address>`, `--kind durable|virtual` and
  `--require <capability>` (repeatable; `metal3`, `durable`, `virtual`,
  `unity`, `unreal`, `xcode`, `cua-driver`, `claude-code`, `codex`,
  `screen-recording`, `ci-idle`) filter the list.
- The exit status is 0 when any instance answered and 2 when none did.
- `machctl list` and `machctl images` print one warning line above an
  instance's rows while its catalog is `stale` or `unavailable`, naming the
  failing file, the error and the time of the failure.

`machctl acquire` takes one of three forms. `--image <image>` (the default,
`macos26`) leases a virtual machine from the local instance, or from the one
`--instance <ID or address>` names. `--machine <name>` leases a named durable
machine from the instance that lists it. `--require <capability>` (repeatable,
the names `list` accepts) lists every instance, keeps the machines and offers
with every capability, and prefers an available durable machine whose CI runner
is idle, then any available durable machine, then an acquirable offer. Before
each attempt it prints the choice on stderr, for example
`chose mac-b (durable, metal3, CI idle)`, and when the owner refuses
for capacity (`durable machine leased`, `durable machine unavailable`, `macOS
VM limit reached`) it tries the next candidate. The forms exclude one another,
and the flags that shape only a virtual machine (`--network`, `--profile`,
`--cpu`, `--memory-mb`, `--disk-gb`, `--no-wait`, `--vnc`, `--source`,
`--expected-source-fingerprint`) are refused with `--machine` and `--require`.

`status`, `heartbeat`, `release`, `wait`, `exec`, `push` and `pull` send the
command to the instance that lists the machine. `<vm>@<instance>` names the
instance, and is required when two instances list the name. A name no instance
lists goes to the local instance.

- The CLI remembers the leases it acquired in `cli-leases.json` in the state
  folder (`$MACHINE_SERVICE_STATE`, else `~/.local/state/machine-service/`;
  mode 600, changed under a lock and replaced by a rename), keyed by machine
  and instance. `exec`, `push`, `pull`, `heartbeat` and `release` send the
  remembered `lease_id`, which a durable machine requires; `--lease-id`
  overrides it, and a release forgets it.
- `push` and `pull` send the bytes over HTTP whenever the machine's owner is
  another instance or not on loopback, and with `--bytes` on loopback
  ([Byte transfers](#byte-transfers)). A folder travels as a tar stream, and a
  pull lands through a temporary sibling only after its digest matched.

A selected environment (`--environment` or `VM_ENVIRONMENT_FILE`) keeps every
command on its own instance and never asks Consul.

(The installer links `~/.local/bin/machctl` to
`~/.local/libexec/machine-service/machctl`. launchd starts the daemon, so
`machine-service` does not need to be on PATH.)

## Byte transfers

A host path means nothing to an instance on another Mac, so two endpoints carry
the bytes themselves, for both kinds of machine (FD-4):

- `PUT /machines/<name>/files?lease_id=<id>&path=<remote path>[&name=<item>]`
  takes a file's bytes with `Content-Type: application/octet-stream`, or a
  folder as a tar stream of its contents with `Content-Type:
  application/x-tar`, and the body's SHA-256 (hex) in `X-Content-SHA256`.
  `name` is the item's name when `path` is an existing folder (default: the
  last part of `path`). The answer is the path transfer's JSON with `bytes`,
  `sha256` and `content` (`file` or `directory`).
- `GET /machines/<name>/files?lease_id=<id>&path=<remote path>` answers a
  file's bytes or a folder's tar stream, with the digest in
  `X-Content-SHA256` and the item's name in `X-Content-Name`.
- The instance checks the machine and, for a durable machine, the lease before
  it keeps a byte. It stages the body under `<state>/transfers/<id>/`,
  refuses a body whose digest differs (400, nothing written), and then runs
  the path transfer it already has on the staged path: the local file system
  for the durable machine, the guest link for a virtual machine. The staging
  folder is removed when the request ends.
- A tar stream carries regular files and folders only. One that names an
  absolute path, a `..` component, a link or a device is refused whole.
- Path-based `push` and `pull` serve loopback clients only, for both kinds of
  machine.

## acquire semantics

- `purpose` becomes the VM name: `pilot-mac-<purpose>-<rand>` (macOS) or
  `pilot-<purpose>-<rand>` (Ubuntu). One active lease per purpose+image.
- `env` (aliases `pack`/`lane`): `default` (default) injects the env pack
  `~/.config/vm-credentials/default/` (gateway env vars — see
  `pilot-images/profiles/pi/litellm.env.example.md`); `none` for
  clones without application secrets; or an explicit pack name. Every new
  lease still receives its own SSH public key for service access.
- `wait: true` (default) returns only after bootstrap public-key installation,
  fresh key-only command access, a checksum-verified upload/download round trip,
  and any env-pack injection succeed. `wait: false` uses a shorter bootstrap
  deadline and one command probe, but never bypasses key provisioning/readiness.
  Cloning a 200 GB-provisioned macOS base takes minutes; allow a ≥10-minute
  client timeout.
- `ttl_hours` (default 24, cap 720): lease lifetime. Long jobs must heartbeat.
- `POST /machines/<name>/heartbeat` renews the TTL from now and clears any grace
  state. With `ttl_hours` it renews for that many hours; without it, the lease
  is renewed for its own initial `ttl_hours`. The recorded initial TTL is not
  changed.
- A heartbeat on a `running` lease whose VM Tart reports stopped or absent
  fails, and the TTL is not renewed. GC releases such a lease after two
  consecutive passes (about two minutes) that find its VM not running.
- Failure rolls back: stop, delete, verify absence, then remove the key and lease.
  Failed teardown retains the `releasing` record and credentials for retry.
- A daemon restart interrupts any acquisition in progress. At startup, before
  the HTTP server binds, the daemon releases every `pending` or `provisioning`
  record left by the previous process and logs each one, so a retry with the
  same purpose is not refused as already leased.
- If the daemon cannot write a successful acquisition response because the
  client's connection was reset, it releases the new lease and logs it. A
  client that closed its connection normally may still receive the bytes into
  its socket buffer; TTL and GC remain the safeguard in that case.

See [per-lease SSH keys](docs/lease-ssh-keys.md) for bootstrap trust, key-only
OpenSSH options, recovery, and the legacy-lease rollout requirement. The running
shared daemon is not restarted automatically by a source change.

## pi-subagents integration

Register `examples/pi-subagents/vm-use.workflow.js` as a named workflow resource
or paste it as `workflowScript`. It guarantees release even when the child
agent fails, and requires evidence to be pulled to `/var/tmp/` before teardown.

## State & files

- `~/.local/state/machine-service/state.json` — all leases
- `~/.local/state/machine-service/state.lock` — cross-process flock
- `~/.local/state/machine-service/service.log` — daemon log
- `~/.local/state/machine-service/ssh/<vm>/` — private per-lease identity and known_hosts;
  never copy this directory into public reports or guest workspaces
- `/tmp/tart-run-<vm>.log` — per-VM console log (same convention as pilot-images)
- `~/.local/state/machine-service/durable.json` — the current durable lease
- `~/.local/state/machine-service/leases/<lease_id>/` — a durable lease's
  `lease.json`, `sessions.json`, `inventory-before.json`,
  `inventory-after.json`, `changes.json` and `hygiene.json`, kept 30 days
- `~/.local/state/machine-service/leases/<lease_id>/ci-busy.json` — the CI
  samples of a durable lease
- `~/.local/state/machine-service/transfers/` — byte transfers in flight,
  removed when each request ends
- `~/.local/state/machine-service/cli-leases.json` — the leases `machctl`
  acquired, written by the CLI; the installer's move of an old vm-service
  state folder keeps it
- `~/machine-service/leases/<lease_id>/` — a durable lease's workspace
- `/var/tmp/com.wezzard.machine-service/durable-machine.lock` — the host-wide
  durable lease lock, shared by every instance on the Mac
- `~/.config/machine-service/agents/` — the owner's agent keys and
  `endpoints.json`

## Roadmap

- **Image tools in machine-service.** Images are built, maintained and updated
  today with the separate pilot-images scripts. In the future, machine-service
  should provide its own tools to build, maintain and update images, so a Mac
  can keep its bases current without a hand-run script.

## Not implemented here (deliberately)

- **Task registry in AGENTS.md** — the service records leases, but the human
  registry table is maintained by the acquiring agent, as AGENTS.md requires
  ("MUST register your task ... before task started").
- **No bridged networking, no base/seed/work boots, no credential writes into
  golden images** — refused by design, not by convention.

## License

MIT. See [LICENSE](LICENSE).
