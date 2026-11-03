# Federated catalog questions and image acquisition

## Problem

- A client Mac whose own machine-service instance offers nothing saw no images and no applications, and could not acquire a virtual machine, although the instances on the VM hosts listed them.
- Reproducer, run from such a Mac on 2026-11-03: a VM host's own `/applications` listed Xcode in `macos26` and `macos27`; the local instance listed none; `machctl images` printed nothing and exited 0; the relay's `relay_search {name: "Xcode", os: "macos"}` answered `{"applications":[],"truncated":false}`; `relay_probe` reported only the local instance. Asking the VM host instead of the local instance made Xcode appear. The data existed; only the choice of instance was wrong.

## Root cause

1. The search contract and the relay were designed when machine-service ran on one Mac, beside the relay, so the loopback instance was the whole lab.
2. FD-3 defined aggregation only for `GET /machines`, and FD-4 covered work through a machine's owner. Nothing defined how a catalog question or an image acquisition is answered across instances. machctl chose across instances only inside `acquire --require` (CL-2) and `list`.
3. CN-1 extended the relay only for durable machines, so image acquisition and every catalog question stayed on the loopback instance.
4. Since DC-93 a Mac may run an instance that offers nothing. That instance answers successfully with an empty catalog, which FD-3's own rule says must never be mistaken for a network without machines.

## Changes

| # | Change | Repository | Makes true |
| --- | --- | --- | --- |
| 1 | `questions.rs`: ask a path of every gathered instance at once, keep each answer unchanged under its instance, keep unreachable instances; the `images` table per instance; the image candidates for an acquisition | vm-service | [FD-7](../docs/arch/federation.md#fd-7-catalog-questions-and-image-acquisition), [CL-4](../docs/ux/cli/list.md#cl-4-catalog-questions) |
| 2 | `machctl images`, `images-show`, `capabilities`, `acquisition-capabilities` ask every instance, with `--local` and `--instance`; JSON is one object per instance; exit 2 when none answered; `--reload` on every instance asked | vm-service | CL-4, DC-1xx-FED-2 |
| 3 | `machctl acquire --image` without `--instance` chooses an instance that offers the image, prints why, passes capacity refusals on | vm-service | FD-7 (image acquisition), [CL-2](../docs/ux/cli/list.md#cl-2-acquire-by-capability), DC-1xx-FED-3 |
| 4 | `--instance` accepts a `machine_id` | vm-service | FD-7, [MI-1](../docs/arch/identity.md#mi-1-the-permanent-id-and-the-name), DC-1xx-FED-4 |
| 5 | FD-3 and FD-2 extended to catalog questions; FD-7 written; CN-1 updated; TL-6 exception recorded | vm-service | [FD-3](../docs/arch/federation.md#fd-3-aggregation), [CN-1](../docs/arch/consumers.md#cn-1-mcp-relay), [TL-6](../docs/arch/test-layers.md#tl-6-live-durable-machine), DC-1xx-FED-5, DC-1xx-FED-6 |
| 6 | `discoverInstances`: one discovery for questions, image acquisition and durable owners; isolation refusal reported, never thrown | mcp-relay | mcp-relay `docs/instances.md` §2, DC-7x-FED-1, DC-67 |
| 7 | relay_search per instance with installation `instances` and catalog `generation`; relay_probe and relay_acquisition_capabilities per instance; `instance` argument | mcp-relay | `docs/instances.md` §3, `docs/search-contract.md` "Every instance", DC-7x-FED-2, DC-7x-FED-3 |
| 8 | Image acquisition chooses among instances in FD-7's order, records `chosen`, passes capacity refusals on; `instance` pins it | mcp-relay | `docs/instances.md` §4, FD-7, DC-7x-FED-4 |
| 9 | A VM lease on another instance keeps its owner by `machine_id`, sends files as bytes, checks destruction in the owner's `/machines`, uses the owner's broker | mcp-relay | `docs/instances.md` §5 and §6, `docs/relay-agent-link.md` §3, [MI-5](../docs/arch/identity.md#mi-5-clients), DC-7x-FED-5 |
| 10 | The stale "a CI job may share its desktop" wording in the relay's instructions and relay_acquire's description, ported from the lab branch | mcp-relay | mcp-relay `docs/durable-machines.md` §7 |

## Verification

- Component tests: vm-service `machctl/tests/questions.rs` (a client Mac that offers nothing, two VM hosts with different catalog generations, a stopped instance, through a fake Consul agent) and mcp-relay `tests/federation.test.ts` (the same lab through the injected Bonjour seam), plus the updated component tests that read these commands' output.
- The reproducer is kept as two opt-in, read-only live checks that find the instances at run time and name none (DC-1xx-FED-6, DC-7x-FED-6):
  - `MACHINE_SERVICE_TEST_LIVE=TL-6 cargo test -p machctl --test questions_live -- --ignored` reads each discovered instance's `/images` directly, then requires `machctl images --json`, with a local instance that is a closed port, to list every such image under its instance, and `machctl acquisition-capabilities` to answer for each. On the code before change 2 it fails: `machctl images` cannot reach the local instance and exits 2.
  - `npx tsx scripts/live-federation.ts --name Xcode --os macos <outDir>` in mcp-relay reads each discovered instance's `/applications` directly, then requires the built relay, whose loopback instance answers an empty catalog, to name every instance that lists Xcode in relay_search and to answer relay_acquisition_capabilities for each. On the relay before change 7 it fails with `{"applications":[],"truncated":false}`.
- Both live checks were run before and after the change on 2026-11-03: each failed on the old code and passed on the new.
