# Guest session (GS)

> The [relay agent](relay-agent.md) replaces this design's two channels for actions, because both start the agent outside the desktop session. Its frames, checksums and process-group rules carry over ([RA-9](relay-agent.md#ra-9-relation-to-the-guest-session)).

## Why

- Each exec starts one `ssh` process and each push or pull one `scp -r` process (`ssh.rs`). Each pays a new SSH handshake and key exchange.
- A measured mcp-vm-relay action made 16 such requests at a median of 160 ms each; 3 of them carried the action. The VM's own work was 1.14 s of a 4.1 s action.
- A guest session pays the handshake once per lease. Later operations cost one round trip on an open stream.

## GS-1: One stream per lease, opened on demand

- vm-service keeps at most one guest session per lease. The first exec, push or pull opens it; release and destroy close it.
- Operations on one VM already run one at a time under the per-VM operation lock (`service.rs`), so one session serves one request at a time. Frames still carry request IDs, so the protocol can later serve concurrent requests without a version change.
- A session idle for more than 10 s is checked with a ping before reuse. A session that fails the check is closed and replaced before the request is sent.
- The agent sends an alive frame every 2 s and announces the interval in its hello. vm-service takes the link for dead after 5 intervals (10 s) with no byte from the agent; bytes of a frame still arriving count, so a slow link is not taken for dead.
- A dead link fails the request in progress at once, by the rules of GS-5, instead of waiting for the request's own timeout. A watchdog also ends the channel, which frees a request blocked while sending.
- A guest that is suspended sends nothing, so its session counts as dead after the same 10 s. The next request opens a new session.
- The SSH channel sets no SSH keepalive; the alive frames cover the session at every layer the channel crosses.

## GS-2: Two channels, one protocol

- A channel is a command whose standard input and output become the session's byte stream. The client, the protocol and the agent are the same for every channel.
- SSH stdio channel: `ssh <lease key options> -T <user>@<ip> <start command>`. It uses the lease's existing identity, host-key pin and `ClearAllForwardings=yes`; it adds no new authority.
- Tart exec channel: `tart exec -i <vm> <start command>`. It reaches the guest over vsock through the Tart guest agent (Tart 2.40.1 or later), so it does not depend on the guest network or sshd.
- Every channel must start the agent as the lease's SSH user with that user's login environment, because commands sent today inherit that environment from `sshd`.
- `MACHINE_SERVICE_GUEST_LINK` selects the link: `ssh` (one SSH connection per operation, the current behaviour), `ssh-session` or `tart-session`. The [conformance suite](test-layers.md#tl-4-live-vm) decides which becomes the default.

## GS-3: The guest session agent

- `guest-session-agent` is a native binary in the `guest-session` crate. vm-service streams the build matching the guest's kind (`guest-session-agent-<kind>`, or `VM_GUEST_SESSION_AGENT`) to `$HOME/.vm-service-session/agent`, with the same atomic, private upload as the guest console agent.
- On start the agent sends a hello frame carrying the protocol version and the SHA-256 of its own executable. When the hash differs from the build vm-service holds, vm-service uploads the agent again and reopens the session.
- Agents are release builds, so the upload on a lease's first session stays small.
- The start command exits 127 when no agent is installed. vm-service then uploads the agent once, over the lease's ordinary `ssh` with the bytes on standard input, and starts it again. A lease's first session therefore costs three channel commands: the start that finds no agent, the upload, and the start that serves the session. A second failure to start is reported as an error and nothing runs.
- The agent runs a command line through `$SHELL -c`, the way `sshd` runs a remote command, so quoting, `PATH` and exit codes match the current exec. Like `sshd`, it names the shell by its file name, so the shell's own messages read `bash: …`, not `/bin/bash: …`. A script is fed to `zsh -s` (macOS) or `bash -s` (Linux) on standard input, as today.
- The agent starts a command only after its standard input has arrived whole and matched its SHA-256, and renames a written file into place only after its bytes matched. Until then a request has no effect in the guest, which gives each request one commit point: the host's end frame.
- Each command runs in its own process group. A timeout, a `cancel` or the end of the session kills the whole group.
- The agent writes files to a temporary sibling and renames it into place, so a reader never sees a partial file.
- The agent exits when its input closes. When the agent exits, it kills the process groups of commands still running.

## GS-4: Frames

All integers are big-endian.

| Field | Size | Meaning |
| --- | --- | --- |
| magic | 2 | `0x5653` |
| type | 1 | frame type, below |
| flags | 1 | 0 |
| request | 8 | request ID; 0 for the session itself |
| length | 4 | payload length, at most 1 MiB |
| header checksum | 4 | CRC-32 (IEEE) of the 16 bytes above |
| payload | length | JSON for control frames, raw bytes for data frames |
| payload checksum | 4 | CRC-32 (IEEE) of the payload |

- The header has its own checksum, so a corrupted length is detected before the reader waits for bytes that will never come.

| Type | Direction | Payload |
| --- | --- | --- |
| hello | agent → host | `{protocol, build, os, heartbeat_ms}` |
| alive | agent → host | empty; sent every `heartbeat_ms` (GS-1) |
| ping, pong | both | 8-byte nonce |
| exec | host → agent | `{command, stdin, timeout_s}`; data frames with the standard input, possibly none, and an end frame follow |
| write | host → agent | `{path, name, mode}`; data frames and an end frame follow |
| read | host → agent | `{path}`; the agent answers with data frames and an end frame |
| data | both | raw bytes |
| end | both | `{size, sha256}` of the data frames before it |
| output | agent → host | raw bytes of the command's combined standard output and error |
| exit | agent → host | `{rc, signal, timed_out, cancelled}` |
| done | agent → host | `{size, sha256}` of what the agent wrote |
| cancel | host → agent | empty; ends a running exec |
| error | agent → host | `{code, message}`; ends the request |

- A bad magic, an unknown type, an oversized length or a checksum mismatch ends the session. The request in progress fails loudly; nothing partial is reported as success.
- Every file transfer ends with the size and SHA-256 of all its bytes, checked by the receiver. A truncated stream, such as the one Tart issue #1347 reports for `tart exec` above about 256 KiB on macOS 12–15 guests, cannot pass as complete.

## GS-5: Failure outcomes, at most once

| When the session fails | exec | write (push) | read (pull) |
| --- | --- | --- | --- |
| Before the request's commit point | reopen and send once | reopen and send once | reopen and send once |
| After the commit point, before the result | uncertain; not resent | uncertain; not resent | resent once on a new session |
| Checksum or protocol error | uncertain; not resent | uncertain; not resent | resent once on a new session |

- An uncertain result is an HTTP error whose body carries `"outcome": "uncertain"`, so a client can tell it apart from a command that ran and failed.
- A read is resent because reading has no effect in the guest.
- A link that falls silent (GS-1) is a session failure like any other. Its outcome depends on where the request was, as the table shows.
- A session watches its VM's release flag. A release that finds a command running sends `cancel`; the agent kills the command's process group and answers with its exit. When no exit arrives within 5 s, vm-service closes the session, which makes the agent kill what is left. The exec reports that it was cancelled by release, as today.
- A release during a push or pull closes the session, which closes the agent's input; the agent deletes an unfinished file. After a release the session refuses every new request, and vm-service closes it.

## GS-6: Scope of the first version

- The HTTP API of exec, push and pull keeps its routes and fields. A client sees the same JSON, with these differences:
  - A push or pull answered by the session adds `bytes` and `sha256`.
  - The session keeps standard output and standard error in the order the command wrote them. The `ssh` link gives all standard output first, then all standard error.
  - A command ended by a signal answers `rc` 255, as `ssh` does, and adds `signal`.
  - A command past its timeout answers the error `ssh failed: the command timed out after <t>s and was killed`, where the `ssh` link answers `ssh failed`.
- A push of a host directory, or a pull of a guest directory, still uses `scp -r`. The agent answers a directory read with the error `is_directory`.
- The request carries the result's SHA-256 back from the agent, so a later client can drop its separate integrity requests. mcp-vm-relay's own request sequence is designed in its repository.
