# Remote session (RS)

> The [broker](broker.md) runs this protocol twice: once with each client and once with each relay agent ([BK-3](broker.md#bk-3-two-sessions-and-one-execution-record)).

## Why

- Today a client sends one HTTP request per operation and waits on that TCP connection for the result, for up to 660 s in mcp-vm-relay. When that connection breaks, the client cannot learn whether the operation ran.
- Across the internet or an intercontinental intranet, connections break, NAT and proxies drop idle connections silently, and client addresses change. SSH has no resumption: a broken SSH connection loses its channels.
- The remote session follows the pattern of VS Code's reconnecting protocol, SignalR stateful reconnect and MQTT 5 sessions: sequence numbers, acknowledgements, a replay buffer and a grace period, carried over WebSocket. WebSocket passes HTTPS proxies on port 443, which is why Kubernetes moved its streaming from SPDY to WebSocket (KEP-4006).

## RS-1: Transport

- vm-service accepts remote sessions on its own listener, `MACHINE_SERVICE_SESSION_PORT` (default 6241), at path `/session`. A separate listener lets vm-service set read timeouts and shut down a dead socket, which the HTTP server's upgraded stream does not allow.
- Off loopback, the listener requires TLS and a client credential: a bearer token, or a client certificate (mutual TLS). On loopback, plain WebSocket is allowed for local clients and tests. The listener binds all interfaces when `MACHINE_SERVICE_SESSION_PORT` is set and a credential is configured, so clients on other machines can connect (DC-10).
- vm-service terminates TLS itself with rustls, with no proxy in front. A proxy would hold its own connection timeouts, so a dead client connection could stay open in the proxy after vm-service has given up on it, or the other way round.

## RS-2: Session, sequence and acknowledgement

- The client's first message is `hello {session, received}`. Without `session`, vm-service creates a session. With a known `session`, vm-service resumes it.
- vm-service answers `welcome {session, resumed, received, heartbeat_ms, grace_ms}`.
- Every later message is an envelope `{seq, ack, body}`. `seq` counts up from 1 in each direction. `ack` is the highest sequence number received without a gap.
- Each side keeps the messages the other has not acknowledged. On resume, each side resends those with `seq` above the other's `received`.
- A receiver drops a message whose `seq` it has already received. A gap in `seq` is a protocol error and closes the connection.
- The session is not tied to a TCP connection, so it survives a new source port or a new client address.

## RS-3: Operations and execution IDs

- `call {call, op, execution, args}` asks for one operation (`exec`, `write`, `read`, `status`, `heartbeat`, `release`). `execution` is the client's execution ID.
- `result {call, ok, value | error}` answers it. File bytes travel in binary WebSocket messages, each prefixed with the 8-byte call number, and end with the size and SHA-256 of all of them, as in [guest session frames](guest-session.md#gs-4-frames).
- vm-service records every execution ID it has accepted, with the outcome when it has one, for at least 1 hour. A call with a recorded execution ID never runs again: vm-service returns the recorded outcome, or attaches the call to the operation still running.
- `query {execution}` returns `completed` with the result, `running`, or `unknown`. `unknown` means vm-service has no record, for example after it restarted; the client reports the operation as uncertain and does not resend it.

## RS-4: Heartbeat and dead-connection detection

- Each side sends a WebSocket ping after 5 s without traffic. A side that receives nothing for 15 s treats the connection as dead and closes it.
- The client then reconnects with exponential backoff from 250 ms up to 5 s, with random jitter, and resumes.

## RS-5: Grace period

- vm-service keeps a disconnected session for its grace period, 10 minutes by default. Running operations continue and their results wait in the replay buffer.
- When the grace period ends, vm-service drops the session and its buffer. Operations keep their lease semantics: a running command is not cancelled by the loss of a client. Their outcomes stay available to `query` by execution ID.
- A client that resumes an expired session receives `expired` and starts a new session. It queries the execution IDs it had in flight.

## RS-6: Scope

- The HTTP API stays. A remote session is a second way to reach the same operations, so local clients keep working unchanged.
- The mcp-vm-relay client is built and tested in the mcp-vm-relay repository. The cross-layer tests in this repository use a Rust test client that follows the same protocol.
