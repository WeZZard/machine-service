# Broker (BK)

> The client-facing wire contract, precise enough to build a client from, is the [relay protocol](relay-protocol.md) (RP).

## Why

- A relay agent behind NAT cannot accept a connection, and a client in another site cannot accept one either, so both open outgoing connections to a place that both can reach.
- This is the pattern of the GitHub Actions self-hosted runner, which polls GitHub over HTTPS, and of the AWS Systems Manager agent, which keeps an outgoing WebSocket open to the Session Manager service.
- machine-service is already the place that clients discover and reach, so the broker is a part of machine-service, and the operator runs no extra server (DC-99).

## BK-1: Endpoint

- The broker accepts WebSocket connections on the remote session listener, `MACHINE_SERVICE_SESSION_PORT` (6241 by default), at path `/relay`, under TLS ([RS-1](remote-session.md#rs-1-transport)). The listener binds every interface, because the guests reach it on their network's host address (BK-5); it accepts nothing but authenticated sessions.
- Relay agents and clients connect to the same endpoint, and the first message of a session says which role it takes: `attributes.role` in the hello is `agent` or `client` ([RP-3](relay-protocol.md#rp-3-roles-and-credentials)).
- machine-service runs the broker when `MACHINE_SERVICE_GUEST_LINK=relay`. Until the relay acceptance passes (M7), SSH stays the default link and the relay link is chosen by the operator (DC-141). An instance told to run it that cannot open the listener does not start.
- The broker's TLS comes from a per-instance authority kept in the state folder, `relay/tls/`. It signs a certificate for the TLS server name each client sends, and for the host's own addresses when a client sends none, so the broker is reached by any of its names (TC-6). Clients pin the authority by `relay_ca_sha256` ([RP-1](relay-protocol.md#rp-1-endpoint-and-trust), DC-143).

## BK-2: Pairing and routing

- A relay agent registers with its agent credential and its first relay message, its hello; the broker binds the session to that lease or that durable machine. A newer agent session of the same lease replaces the older one.
- A client connects with its lease credential, and the broker routes the client's operations to the relay agent of that lease. A virtual machine's lease has one agent credential and one client credential, made at acquisition; the client credential is in the acquisition's answer only, and the lease record keeps only the SHA-256 of both, from which a restarted broker accepts them again.
- A durable machine's relay agent holds one credential per machine, `relay/durable-agent.credential` in the state folder (mode 600), which the instance writes into its own agent's configuration ([IN-3](installation.md#in-3-the-durable-macs-relay-agent)); a durable lease's client credential is good while that lease is the machine's current lease.
- A credential may carry an end, which the broker checks on every `hello`. machine-service sets it from the lease: the end of its TTL plus its grace bounded by the TTL, moved at every renewal ([RP-3](relay-protocol.md#rp-3-roles-and-credentials), DC-214).
- The broker answers `pending` with code `agent_not_connected` to a client whose relay agent has no live session, and keeps the operation waiting for `MACHINE_SERVICE_RELAY_AGENT_WAIT` (600 s by default, the agent session's grace period) before it reports the operation as not started ([RP-8](relay-protocol.md#rp-8-when-the-relay-agent-is-not-connected)).
- When a relay agent's session ends while calls are out, the broker sends them again, with their execution IDs, to the agent's next session; the relay agent answers each from its record, so none runs twice. An upload continues from the agent's committed offset: the broker continues one it reads itself, and a client continues its own after `upload_incomplete` ([RP-15](relay-protocol.md#rp-15-resumable-streams)).
- Release and expiry revoke the lease's credentials as the lease starts releasing: its client sessions receive `revoked` and are closed, its agent session is closed, and every later hello is refused ([RP-10](relay-protocol.md#rp-10-revocation)).
- A broker serves the virtual machines of its own Mac and, when exposure is on, its own durable machine. One broker never routes to another broker's machines (DC-30).

## BK-3: Two sessions and one execution record

- A client's session with the broker and a relay agent's session with the broker are two independent resumable sessions of [RS](remote-session.md).
- The broker records every execution ID that it accepted, per lease, with its outcome when it has one, for 1 hour after the outcome, as [RS-3](remote-session.md#rs-3-operations-and-execution-ids) requires. A record keeps an exec's or script's output up to 16 MiB, so a later caller with the same ID gets the whole output from the record; past that, the broker asks the relay agent to replay it.
- The broker passes a stream through and keeps none of it: an upload's chunks go to the relay agent as they come, within the 32 MiB window beyond the agent's `progress`, and the agent's streams go to the client as the client's session takes them. A broker holds about one window per stream, whatever the stream's size; the relay agent holds an upload in a spool named by its execution ID, checks it whole against its end, and lets a sender continue it from its committed offset after any loss ([RP-15](relay-protocol.md#rp-15-resumable-streams), DC-195). A client of protocol 1 still sends an upload whole, which the broker holds up to 256 MiB.
- The relay agent records execution IDs as well ([RA-5](relay-agent.md#ra-5-operations)), so a resend after either session resumes, or after a broker restart, never runs a command twice.
- A client asks `query {execution}` after any loss and receives `completed`, `running` or `unknown`; the broker answers from its record, else from the relay agent's.
- A target may have a **launcher**, which starts every exec and script that the target's clients send (DC-219). For a call that makes a new record, the broker asks it for the operation the relay agent runs instead, outside the records' lock; the record keeps the client's operation for queries, and tells the launcher when it settles, so what the launch left is removed. A call that attaches to a record is never launched again, and a launch that fails is refused with `launch_failed` and keeps no record. machine-service gives its durable machine's target a launcher, so a relay client's command on a durable lease runs as the lease's own ([DM-4](durable-machine.md#dm-4-exec)); a virtual machine's target has none.
- machine-service's own HTTP `exec`, `push` and `pull` on a `relay` lease, and so `machctl`'s, are calls on the same records, made in-process. The HTTP body may carry `execution_id` to make a request idempotent, and the answer names its `execution_id` and `link: relay`. A folder travels as a tar stream through `write_tree` and `read_tree` ([RP-13](relay-protocol.md#rp-13-folders)), so no action on a `relay` lease uses SSH.

## BK-4: Discovery of the broker

- machine-service reports its broker URL as `relay_url`, and its authority's fingerprint as `relay_ca_sha256`, in `/health`, in its Bonjour TXT record and in its Consul metadata, which `machine-service-install catalog-description` writes ([FD-1](federation.md#fd-1-advertisement-and-registration)). `GET /relay/ca.pem` serves the authority.
- By default, `relay_url` names the Mac's Bonjour host name, `<LocalHostName>.local`, which every Mac on the network resolves, and the listener's port (DC-153).
- The operator can set `MACHINE_SERVICE_RELAY_URL` to a URL that other sites can reach, such as a Cloudflare Tunnel hostname or a Tailscale name, and discovery advertises that URL instead ([TC-6](transport-contract.md#tc-6-the-broker-is-reached-by-name)).
- A client never needs an IP address that a person wrote down.

## BK-5: Reaching the broker from a virtual machine

- A guest on Tart's shared NAT network reaches its host's broker through the host's address on that network, which machine-service finds at acquisition as its own interface address on the guest's subnet and writes into the guest's relay agent configuration. `MACHINE_SERVICE_RELAY_GUEST_URL` replaces it, for tests and unusual networks, and `MACHINE_SERVICE_RELAY_GUEST_PORT` replaces only its port, for a test rig's chain in front of the broker (TL-7).
- A guest on the `internet` network is kept off the host by the guest firewall ([DC-14](../decisions.md)), which drops every connection that such a guest opens to the host's own addresses.
- The guest firewall makes one exception: an `internet` guest may open a TCP connection to the broker's port on its host, and to no other port (DC-107). `vm-service-guest-firewall install` writes it for port 6241, `--broker-port` for another and `--no-broker` for none; an `internet` lease on an instance whose session port the installed anchor does not pass is served over SSH (RA-8) and says why in its record.
- The exception keeps DC-13's purpose, because the broker accepts only an authenticated relay agent session under TLS and serves no other service, and the guest still reaches no other port of the host and no private network.
- A host-local channel such as vsock was not chosen, because Tart's vsock transport has dropped guest output on some guests (openai/tart issue 1347), and the relay agent would need a second transport. Reaching the broker only through a public URL was not chosen, because it would make every `internet` lease depend on a tunnel, which is outside the tested scope (DC-101).
