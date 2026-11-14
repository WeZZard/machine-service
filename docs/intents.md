# Intents

The owner's goals, taste and standing defaults for machine-service. This page overrides [decisions](decisions.md) where the two disagree. Each line cites the decisions it comes from.

## Goals

- Agents get machines to work on through leases and never drive Tart or a Mac directly: disposable virtual machines, and the Macs themselves as durable machines (DC-19, DC-22).
- Any Mac that installs `machctl` finds every instance on the network with nothing else installed, and sees every machine they offer (DC-21, DC-84).
- Renaming a Mac breaks nothing that consumes machine-service (DC-86).
- An agent's action runs as the person at the desktop would run it: in the logged-in user's desktop session, with its keychain and window server, not over SSH (DC-97, DC-98).
- The service and relay layer know nothing about privacy permissions. A task that needs one arranges it itself, and a relayed command never inherits the relay agent's (DC-308, DC-309).
- Machines may sit in other sites behind NAT. Both sides dial out, and any network that meets the transport contract works. Only the intranet is built and tested (DC-100, DC-101).
- Durable Macs run Claude Code and Codex through the in-house gateway with a key per machine per agent, so usage is tracked per machine (DC-24).

## Taste

- Choose the best architecture, because migration costs nothing when agents rebuild the stack (DC-1).
- Prefer one coordinated switch with loud refusals to a long compatibility window (DC-33).
- Hygiene on a durable Mac is a pragmatic best effort: it runs as the owner's admin account and reports what it finds (DC-23).
- Speed is best effort, measured and reported, and never fails a run (DC-193).
- Prefer standard practice and common tools, such as one fresh VM per CI job and Toxiproxy with mitmproxy, to inventions (DC-104, DC-259).

## Standing defaults

- A decision is brought to the owner in a document that holds its whole context, its options with their consequences and the evidence, never as a bare question (DC-341).
- The agent makes the technical choices, writes the reason into the design, and reports its concerns, their consequences and its actions in plain language (DC-7).
- Every change ships with unit and integration tests on the layer that owns the behaviour. Live tests run on second instances and never touch the installed instance (DC-2, DC-5, DC-40).
- The final end-to-end relay suite is the acceptor: 6 cells under 4 network profiles, with no SSH (DC-103).
- No in-house endpoint appears in this repository, its defaults or its documents (DC-32).
- Each environment gets its own credentials. A CI VM never receives the agent pack (DC-283).
- The Developer ID key never leaves the owner's keychain, and the owner signs every macOS relay agent release on their own Mac (DC-224, DC-225).
- The CI runner on the durable Macs is never paused (DC-25). Durable test cells run only when the owner dispatches them by hand (DC-263).
- 192.0.2.3 does not expose itself as a durable machine (DC-39, DC-44).
