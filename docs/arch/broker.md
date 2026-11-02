# Broker (BK)

> The client-facing wire contract, precise enough to build a client from, is the [relay protocol](relay-protocol.md) (RP).

## Why

- A relay agent behind NAT cannot accept a connection, and a client in another site cannot accept one either, so both open outgoing connections to a place that both can reach.
- This is the pattern of the GitHub Actions self-hosted runner, which polls GitHub over HTTPS, and of the AWS Systems Manager agent, which keeps an outgoing WebSocket open to the Session Manager service.
- machine-service is already the place that clients discover and reach, so the broker is a part of machine-service, and the operator runs no extra server (DC-99).

## BK-1: Endpoint

- The broker accepts WebSocket connections on the remote session listener, `MACHINE_SERVICE_SESSION_PORT`, at path `/relay`, under TLS ([RS-1](remote-session.md#rs-1-transport)).
- Relay agents and clients connect to the same endpoint, and the first message of a session says which role it takes.

## BK-2: Pairing and routing

- A relay agent registers with its agent credential, and the broker binds the session to that lease or that durable machine.
- A client connects with its lease credential, and the broker routes the client's operations to the relay agent of that lease.
- The broker answers `agent not connected` to a client whose relay agent has no live session, and it keeps the operation waiting for the relay agent's grace period before it reports the operation as not started.
- A broker serves the virtual machines of its own Mac and, when exposure is on, its own durable machine. One broker never routes to another broker's machines (DC-30).

## BK-3: Two sessions and one execution record

- A client's session with the broker and a relay agent's session with the broker are two independent resumable sessions of [RS](remote-session.md).
- The broker records every execution ID that it accepted, with its outcome when it has one, for at least 1 hour, as [RS-3](remote-session.md#rs-3-operations-and-execution-ids) requires.
- The relay agent records execution IDs as well ([RA-5](relay-agent.md#ra-5-operations)), so a resend after either session resumes never runs a command twice.
- A client asks `query {execution}` after any loss and receives `completed`, `running` or `unknown`.

## BK-4: Discovery of the broker

- machine-service reports its broker URL as `relay_url` in `/health`, in its Bonjour TXT record and in its Consul metadata ([FD-1](federation.md#fd-1-advertisement-and-registration)).
- By default, `relay_url` names the Mac's own host name and the listener's port.
- The operator can set `MACHINE_SERVICE_RELAY_URL` to a URL that other sites can reach, such as a Cloudflare Tunnel hostname or a Tailscale name, and discovery advertises that URL instead ([TC-6](transport-contract.md#tc-6-the-broker-is-reached-by-name)).
- A client never needs an IP address that a person wrote down.

## BK-5: Reaching the broker from a virtual machine

- A guest on Tart's shared NAT network reaches its host's broker through the host's address on that network.
- A guest on the `internet` network is kept off the host by the guest firewall ([DC-14](../decisions.md)), which drops every connection that such a guest opens to the host's own addresses.
- The guest firewall makes one exception: an `internet` guest may open a TCP connection to the broker's port on its host, and to no other port (DC-107).
- The exception keeps DC-13's purpose, because the broker accepts only an authenticated relay agent session under TLS and serves no other service, and the guest still reaches no other port of the host and no private network.
- A host-local channel such as vsock was not chosen, because Tart's vsock transport has dropped guest output on some guests (openai/tart issue 1347), and the relay agent would need a second transport. Reaching the broker only through a public URL was not chosen, because it would make every `internet` lease depend on a tunnel, which is outside the tested scope (DC-101).
