# Remote session (RS)

> The [broker](broker.md) runs this protocol twice: once with each client and once with each relay agent ([BK-3](broker.md#bk-3-two-sessions-and-one-execution-record)). The exact wire form, precise enough to build a client from, is the [relay protocol](relay-protocol.md) (RP-2); the `remote-session` crate implements it.

## Why

- Today a client sends one HTTP request per operation and waits on that TCP connection for the result, for up to 660 s in mcp-vm-relay. When that connection breaks, the client cannot learn whether the operation ran.
- Across the internet or an intercontinental intranet, connections break, NAT and proxies drop idle connections silently, and client addresses change. SSH has no resumption: a broken SSH connection loses its channels.
- The remote session follows the pattern of VS Code's reconnecting protocol, SignalR stateful reconnect and MQTT 5 sessions: sequence numbers, acknowledgements, a replay buffer and a grace period, carried over WebSocket. WebSocket passes HTTPS proxies on port 443, which is why Kubernetes moved its streaming from SPDY to WebSocket (KEP-4006).

## RS-1: Transport

- machine-service accepts remote sessions on its own listener, `MACHINE_SERVICE_SESSION_PORT` (default 6241), at path `/relay` (BK-1, DC-115). A separate listener lets machine-service set read timeouts and shut down a dead socket, which the HTTP server's upgraded stream does not allow.
- The listener requires TLS and a bearer credential in the session's first message; no client certificate is asked for, because a proxy may end TLS on the way (TC-5, DC-113). Plain WebSocket is allowed only on a loopback connection, for local tools and tests (DC-114). The listener binds all interfaces, because the guests reach it on their network's host address (BK-5, DC-10).
- machine-service terminates TLS itself with rustls, with its own per-instance authority ([RP-1](relay-protocol.md#rp-1-endpoint-and-trust)). A deployment may still put a TLS-ending proxy such as a Tunnel in front for other sites (TC-5); each side's heartbeat and dead-connection bound then still see the whole path, because the heartbeat is a protocol message (RS-4).

## RS-2: Session, sequence and acknowledgement

- The client's first message is `hello {version, session, received, credential, attributes}`; `attributes.role` is `client` or `agent` (RP-3). Without `session`, machine-service creates a session. With a known `session`, machine-service resumes it.
- machine-service answers `welcome {session, resumed, received, heartbeat_ms, grace_ms}`, or `expired` or `error` (RP-2).
- Every later message is an envelope `{seq, ack, body}`, a binary WebSocket message of two big-endian `u64` and the body (DC-109). `seq` counts up from 1 in each direction. `ack` is the highest sequence number received without a gap.
- Each side keeps the messages the other has not acknowledged. On resume, each side resends those with `seq` above the other's `received`.
- A receiver drops a message whose `seq` it has already received. A gap in `seq` is a protocol error and closes the connection.
- The session is not tied to a TCP connection, so it survives a new source port or a new client address.

## RS-3: Operations and execution IDs

- Operations travel as relay messages in the envelope bodies ([RP-4](relay-protocol.md#rp-4-relay-messages)): `call {call, execution, op}` asks for one operation (`exec`, `script`, `write`, `read`, `cancel`, `status`), and `result {call, execution, replayed, outcome}` answers it. `execution` is the client's execution ID. Bytes travel in binary chunks, each prefixed with the byte `0x01` and the 8-byte call number, and end with the size and SHA-256 of all of them (DC-122).
- The broker records every execution ID it has accepted, with the outcome when it has one, for at least 1 hour, and the relay agent records it until its lease is purged at release (DC-121, DC-173). A call with a recorded execution ID never runs again: the answer is the recorded outcome, or the call attaches to the operation still running ([RP-7](relay-protocol.md#rp-7-execution-ids-records-and-query)).
- `query {call, execution}` returns `completed` with the outcome, `running`, or `unknown`. The broker answers from its record, else asks the relay agent, so a query answers after a broker restart too. `unknown` means neither has a record; the client reports the operation as uncertain.

## RS-4: Heartbeat and dead-connection detection

- Each side sends the protocol's own `ack` message after 5 s without sending, not a WebSocket ping, because a proxy that ends TLS may answer pings itself (DC-110). A side that receives nothing for 15 s, or whose bodies wait 15 s for an acknowledgement that does not advance, treats the connection as dead and closes it.
- The client then reconnects with exponential backoff from 250 ms up to 5 s, with random jitter, and resumes.

## RS-5: Grace period

- machine-service keeps a disconnected session for its grace period, 10 minutes by default. Running operations continue and their results wait in the replay buffer.
- When the grace period ends, machine-service drops the session and its buffer. Operations keep their lease semantics: a running command is not cancelled by the loss of a client. Their outcomes stay available to `query` by execution ID.
- A client that resumes an expired session receives `expired` and starts a new session. It queries the execution IDs it had in flight.

## RS-6: Scope

- The HTTP API stays. A remote session is a second way to reach the same operations, so local clients keep working unchanged.
- The mcp-relay client is built and tested in the mcp-relay repository, from the [relay protocol](relay-protocol.md). The cross-layer tests in this repository use a Rust test client that follows the same protocol (`relay-broker/tests/common`).
