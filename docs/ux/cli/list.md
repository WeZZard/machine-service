# Machine list and acquisition

## CL-1: The machine list

- `machctl list` discovers the instances ([FD-2](../../arch/federation.md#fd-2-discovery)) and prints one table of every machine and every virtual machine offer ([FD-3](../../arch/federation.md#fd-3-aggregation)).
- Rows are grouped by instance, with the local instance first. Inside an instance the durable machine comes first, when the instance exposes one, then the running virtual machines, then the offers.
- An instance with durable exposure off has no durable row (DC-39).
- An offer whose base VM is not built on its Mac (`base_available: false`) is left out of the table, because that Mac cannot create it. `--json` still carries it (DC-92).
- An unreachable instance keeps one row, which names the error, and in Consul mode its Consul health status.
- When the local instance refuses the connection, no service runs on this Mac. Its row has the state `not running` and the text `no machine-service on this Mac`, not `unreachable` (DC-93).
- `--json` prints the merged list as JSON: one object per instance with its `instance`, `address`, `reachable`, `error` and `machines`, where each machine is the record of [MM-3](../../arch/machines.md#mm-3-capability-report).
- `--local` lists only the local instance. `--kind durable` or `--kind virtual` filters by kind. `--require <capability>` filters by capability, as CL-2 describes.
- `--instance <ID or address>` lists one instance. An address that discovery does not find is asked directly.
- The JSON objects also carry `local`, `found_by` (`local`, `bonjour`, `consul` or `address`), `consul` (the instance's Consul health, in Consul mode) and `catalog` (its image catalog status) (DC-63, DC-84).
- A reachable instance with no machine to show, such as one configured to offer nothing, shows one row with the state `none` and the text `provides no machines`, so that it is never missing from the list (DC-93). While a filter is active, an instance with no matching machine is left out, and an unreachable instance is kept.
- `machctl list` exits 0 when any instance answered, and 2 with the hint to start the service when none did.

```text
$ machctl list
INSTANCE         MACHINE                         KIND     STATE      LEASE (PURPOSE, UNTIL)        CHIP / OS           GPU            FREE DISK  CI
mac-a-m4         pilot-mac-x-session-7f3a        virtual  running    x-session, 18:40              macOS 26 (macos26)  mac2,common3   -          -
mac-a-m4         macos26                         virtual  offer      acquirable 1 of 2             macOS 26            mac2,common3   -          -
mac-a-m4         ubuntu2404                      virtual  offer      acquirable                    Ubuntu 24.04        -              -          -
mac-b            mac-b                           durable  leased     unreal-post-process, 21:05    M4 / macOS 26.3     metal3         128 GiB    busy
mac-c            mac-c                           durable  available  -                             M4 / macOS 26.3     metal3         124 GiB    idle
```

- When an instance does not answer, its machines are replaced by one row:

```text
mac-c            -                               -        unreachable  connection refused (Consul: critical)
```

- The `GPU` column prints `metal3` when the machine supports Metal 3, and otherwise its families.
- The `CI` column prints `busy` or `idle` for a Mac with a CI runner and `-` for one without.
- A warning, such as `low free disk` or a missing permission, is printed under its row, indented.
- When an instance's image catalog is `stale` or `unavailable` ([IC-5](../../arch/image-catalog.md#ic-5-a-failed-rebuild)), a warning line above that instance's rows names the failing file and the time of the failure.

## CL-2: Acquire by capability

- `machctl acquire --purpose <slug> --require metal3 --require unreal` asks for any machine that has every listed capability.
- The capabilities a client can require are names over the capability report: `metal3`, `durable`, `virtual`, `unity`, `unreal`, `xcode`, `cua-driver`, `claude-code`, `codex`, `screen-recording` and `ci-idle`.
- A requirement over a fact the report does not know never matches. `claude-code` and `codex` need the program and, where the record reports agent keys, a present key and gateway endpoint. `ci-idle` matches a Mac without a CI runner or with an idle one (DC-63).
- The CLI lists the machines, keeps those that match, and prefers in order: an available durable machine whose CI runner is idle, then any available durable machine, then a virtual machine offer that is acquirable. It acquires from the instance that owns the first candidate and tries the next candidate when that acquisition is refused for capacity.
- `--machine <name>` acquires a named durable machine, and `--image <image>` acquires a virtual machine. `--instance <ID>` chooses the instance for an image, and narrows the instances `--require` chooses among. Without any of the three forms the image is `macos26`.
- `--image` without `--instance` chooses the instance as [FD-7](../../arch/federation.md#fd-7-catalog-questions-and-image-acquisition) sets: the instances whose offer of the image has its base built, acquirable offers first, the local instance first within each group. It prints `chose <image> (virtual, offer on <instance>)` before each attempt and passes to the next candidate on a capacity refusal. Under a selected environment the image is acquired from that environment's instance only.
- The forms exclude one another. The flags that shape only a virtual machine (`--network`, `--profile`, `--cpu`, `--memory-mb`, `--disk-gb`, `--no-wait`, `--vnc`, `--source`, `--expected-source-fingerprint`) are refused with `--machine` and `--require`, because a durable machine refuses them (DC-69).
- The CLI prints the machine it chose and why on standard error, before each attempt, so a person can see the choice and standard output stays the lease's JSON: `chose mac-c         (durable, metal3, CI idle)`. An offer adds `offer on <instance>`.
- A capacity refusal is `durable machine leased`, `durable machine unavailable` ([MM-5](../../arch/machines.md#mm-5-states)) or `macOS VM limit reached`. Any other refusal ends the command with its error. When every candidate is refused for capacity, the command exits 1 naming each refusal (DC-69).

## CL-3: Naming a machine

- `exec`, `push`, `pull`, `heartbeat`, `release` and `status` take a machine name. A durable machine's name is unique. A virtual machine may be written `<vm>@<instance>`, and the CLI requires that form when two instances have a machine of that name.
- The CLI finds the owning instance from the merged list and remembers the `lease_id` of the leases it acquired in `~/.local/state/machine-service/cli-leases.json` (or under `$MACHINE_SERVICE_STATE`), so a person does not type it. `exec`, `push`, `pull`, `heartbeat` and `release` send it; `--lease-id` overrides it. The file maps each lease to its machine and the owning instance's address, is mode 600, is changed under an exclusive lock and replaced by a rename, and a release forgets its lease (DC-72).
- `push` and `pull` to a remote instance send the bytes ([FD-4](../../arch/federation.md#fd-4-working-on-a-remote-instance)). The command line is the same as for a local instance, and `--bytes` sends the bytes to a local one too (DC-78).
- `wait` finds its machine the same way. A name that no instance lists goes to the local instance, whose answer says it is unknown. Two instances listing the name is refused with the `<vm>@<instance>` form in the error (DC-64).
- The console commands stay with the local instance, because their viewer runs on the service host. A selected environment keeps every command on its own instance and refuses `<vm>@<instance>` and `--instance` (DC-64).

## CL-4: Catalog questions

- `machctl images`, `machctl images-show <image>`, `machctl capabilities` and `machctl acquisition-capabilities` ask every instance, as `machctl list` does ([FD-7](../../arch/federation.md#fd-7-catalog-questions-and-image-acquisition)). Each takes `--local`, which asks only the local instance, and `--instance <ID or address>`, which asks only that instance.
- `--json` of `images`, and the output of `capabilities` and `acquisition-capabilities`, is a JSON array with one object per instance asked: `instance`, `machine_id`, `address`, `local`, `found_by`, `reachable`, `error`, `status` (the HTTP status of the answer) and `answer` (the instance's answer, unchanged). An unreachable instance has `reachable: false`, its `error`, and no `answer`.
- `images-show <image>` prints the same array, holding only the instances whose catalog lists the image, with the image's record as `answer`. It exits 1 when no instance lists the image, and the message names the images each instance offers.
- The `images` table groups its rows by instance, with the local instance first, and starts each row with the instance's name. A warning line above an instance's rows names its stale or unavailable catalog (IC-5). An instance that answers with no image shows one row with `provides no images`. An unreachable instance shows one row with `unreachable` and its error, and the local instance that refuses the connection shows `not running`, as CL-1 sets.
- `images --reload` reloads the catalog of every instance asked, then lists them.
- Each command exits 2 with the hint to start the service when no instance answered. `images` exits 1 when an instance that answered has no catalog, or a reload left a catalog other than `current`. `capabilities` and `acquisition-capabilities` exit 1 when no instance that answered answered with HTTP 200.

```text
$ machctl images
mac-laptop       -              provides no images
mac-a-m4         macos26        macos  pilot-macos26-base     ok            running=0/2   acquirable=true
mac-a-m4         ubuntu2404     linux  pilot-ubuntu2404-base  ok            running=0     acquirable=true
mac-b            unreachable    connection refused
```
