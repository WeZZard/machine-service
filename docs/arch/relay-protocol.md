# Relay protocol (RP)

> The client-facing wire contract of the [broker](broker.md). A client that follows this page alone, such as mcp-relay's TypeScript client, can run commands, scripts and file transfers in a lease's desktop session through the [relay agent](relay-agent.md). The relay agent speaks the same messages to the broker; where its side differs, this page says so.

## Layers

| Layer | What it carries | Section |
| --- | --- | --- |
| TLS over TCP | one connection to the broker's port | [RP-1](#rp-1-endpoint-and-trust) |
| WebSocket | an HTTP/1.1 upgrade at `/relay` | [RP-1](#rp-1-endpoint-and-trust) |
| Remote session | JSON control messages in text frames, envelopes in binary frames | [RP-2](#rp-2-the-session) |
| Relay messages | one relay message per envelope body | [RP-4](#rp-4-relay-messages) |

## RP-1: Endpoint and trust

- The broker's URL is `relay_url` from discovery or from the acquisition's answer ([RP-11](#rp-11-discovery-and-acquisition)), for example `wss://studio.local:6241/relay`. The path is always `/relay`; any other path is answered with HTTP 404.
- A client resolves the URL's host name itself on every connection attempt, because the address may change (TC-4, TC-6). It never needs an IP address that a person wrote down.
- TLS: the client offers ALPN `http/1.1` only and sends the URL's host as the TLS server name. It trusts the public web roots and, in addition, the broker's own authority:
  - `relay_ca_sha256` is the lower-case hexadecimal SHA-256 of the DER encoding of the broker's authority certificate. Discovery and the acquisition's answer carry it.
  - The authority's PEM is served by `GET /relay/ca.pem` on machine-service's HTTP port, and the acquisition's answer carries it as `relay.ca_pem`. A client checks that the PEM's certificate hashes to `relay_ca_sha256` before trusting it.
  - The authority is made once per instance and kept in its state folder; it signs a certificate for whatever host name the client sends, and for the instance's own addresses when the client sends no name.
  - A TLS-ending proxy, such as Cloudflare Tunnel, presents its own certificate, which the public roots validate. The proxy's connection to the broker trusts the broker's authority. Nothing in the protocol depends on a client certificate (TC-5).
- Plain `ws://` is accepted only on a loopback connection, for local tools and tests.
- No WebSocket message or frame may exceed 1 MiB (1,048,576 bytes); a larger one closes the connection (TC-8).

## RP-2: The session

The remote session ([RS](remote-session.md)) makes one logical session survive any number of broken connections. Its messages are:

| Message | Frame | Direction | Content |
| --- | --- | --- | --- |
| `hello` | text, JSON | client → broker, first on every connection | `{"type":"hello","version":1,"session":null,"received":0,"credential":"<token>","attributes":{"role":"client","protocol":2}}` |
| `welcome` | text, JSON | broker → client | `{"type":"welcome","session":"<uuid>","resumed":false,"received":0,"heartbeat_ms":5000,"grace_ms":600000}` |
| `expired` | text, JSON | broker → client, instead of `welcome` | `{"type":"expired","session":"<uuid>","reason":"expired"}`, reason `expired` or `unknown` |
| `error` | text, JSON | broker → client, instead of `welcome` | `{"type":"error","code":"unauthorized","message":"…"}`, then the broker closes |
| envelope | binary | both | 8 bytes `seq`, 8 bytes `ack`, both big-endian unsigned, then the body |
| `ack` | text, JSON | both | `{"type":"ack","ack":<n>}`: acknowledges without a body; doubles as the heartbeat |
| `bye` | text, JSON | both | `{"type":"bye"}`: closes the session on purpose; neither side resumes it |

- **Opening.** The first `hello` of a session has `"session": null` and `"received": 0`. The broker answers `welcome` with a new session ID.
- **Sequence numbers.** Each side numbers the envelopes it sends from 1, one by one, per session, not per connection. `ack` in an envelope or an `ack` message is the highest `seq` received without a gap. A receiver drops an envelope whose `seq` it already has; a `seq` beyond the next expected one is a protocol error that closes the connection. An `ack` above the highest `seq` the other side sent is also a protocol error.
- **Replay buffer.** Each side keeps every envelope body until the other side acknowledges it. Acknowledge every body on receipt, in an envelope's `ack` or an `ack` message, independently of handling it.
- **Heartbeat.** Send an `ack` message after `heartbeat_ms` without sending anything (5 s by default; never more than 30 s, TC-3). Treat the connection as dead after 15 s without receiving anything, and also when bodies written on it wait 15 s for an acknowledgement that does not advance. WebSocket pings are not the heartbeat: a TLS-ending proxy may answer them itself.
- **Right after `welcome`.** The broker sends envelopes as soon as it has answered: on a new session the `ready` message ([RP-4](#rp-4-relay-messages)), on a resumed one the bodies above the client's `received`. They may arrive in the same read as the `welcome`, so a client that takes the `welcome` keeps whatever follows it for the session instead of dropping it; a dropped one shows up as a gap at the next envelope.
- **Resume.** After a lost connection, reconnect with exponential backoff (a base that doubles from 250 ms to 5 s, waiting a uniform time between half the base and the base), and send `hello` with `"session": "<id>"` and `"received": <highest seq received>`. The broker answers `welcome` with `"resumed": true` and its own `received`; each side then resends, in order, the bodies above the other's `received` and drops those at or below it. Authenticate every `hello` with the same credential: a session resumes only for the principal that opened it (`error` code `forbidden` otherwise).
- **Expiry.** The broker keeps a disconnected session for `grace_ms` (10 minutes by default). A hello that opens a session may ask for a shorter grace in `attributes.grace_ms`; the broker keeps the shorter of the two for that session and reports it in `welcome` (DC-186). A resume after that, or after a broker restart, is answered `expired`; reason `expired` means the broker dropped it at its grace period, `unknown` means the broker has no record. A client that has not resumed within `grace_ms` plus 15 s may treat the session as expired without asking. On expiry, start a new session and recover the calls that were in flight ([RP-7](#rp-7-execution-ids-records-and-query)).
- **A broken rule** (a gap, an `ack` above the last `seq` sent, an unexpected message) closes only the connection that carried it. The client then resumes the session on a new connection as after any loss, as the `remote-session` crate's client does; the replay restores the order.
- **Refusal codes** in `error`: `unauthorized` (missing, wrong, revoked or expired credential, or a wrong role), `forbidden` (the session belongs to another principal), `protocol` (a broken rule, such as a `hello` with a session of `null` and `received` above 0), `unsupported_version` (a `version` other than 1). A client stops on `unauthorized`, `forbidden` and `unsupported_version`, because a retry cannot succeed.
- Bodies are opaque to the session layer. One body is at most 1,048,560 bytes (1 MiB less the 16-byte envelope header).

## RP-3: Roles and credentials

| Role | `attributes` | Credential | Bound to |
| --- | --- | --- | --- |
| client | `{"role":"client","protocol":2}` | the lease credential, `relay.credential` in the acquisition's answer | that lease's relay agent |
| agent | `{"role":"agent"}` | the agent credential in `~/.config/relay-agent/config.json` | its lease, or its durable machine |

- `attributes.protocol` is the relay protocol the client speaks ([RP-15](#rp-15-resumable-streams)); this page describes protocol 2. A client that leaves it out is taken as protocol 1, whose uploads the broker holds whole, up to 256 MiB. The session's own `version` stays 1.
- `attributes` may also carry `"lease": "<machine name>"`; when it does and names another lease than the credential's, the hello is refused with `unauthorized`.
- Credentials are opaque bearer tokens of 43 URL-safe base64 characters (32 random bytes). The broker stores only their SHA-256.
- A virtual machine's lease gets a new client credential and a new agent credential at acquisition. The broker refuses both from the moment the lease starts releasing or expires ([RP-10](#rp-10-revocation)).
- A durable machine's relay agent holds one agent credential per machine, which the installer writes and the operator rotates. A durable lease's client credential is valid while that lease is the machine's current lease.
- The broker routes a client only to the relay agent of its own lease (BK-2), and one broker never routes to another broker's machines (DC-30).

## RP-4: Relay messages

Every envelope body is exactly one relay message. There are two encodings:

- **Chunk (binary).** The byte `0x01`, then the call number as an 8-byte big-endian unsigned integer, then the data: at most 1,044,480 bytes (1 MiB less 4 KiB). It carries bytes of one call's stream.
- **Control (JSON).** Any other body is a UTF-8 JSON value, never starting with the byte `0x01`. A message with fields is an object with exactly one key, the message's name, whose value holds the fields. A message without fields is the bare JSON string of its name, such as `"heartbeat"`.

The same rule, one key naming the variant or a bare string for a variant without fields, applies to every enumeration below: operations, outcomes and execution states.

### Client → broker

| Message | JSON |
| --- | --- |
| `call` | `{"call":{"call":<u64>,"execution":"<id>","op":<op>,"resume":<bool>,"offset":<u64>}}`; `resume` and `offset` are optional, `false` and `0` when absent ([RP-15](#rp-15-resumable-streams)) |
| chunk | binary, for a call's upload |
| `end` | `{"end":{"call":<u64>,"execution":"<id>","size":<u64>,"sha256":"<hex>"}}`: the size and SHA-256 of the whole stream, from its first byte |
| `query` | `{"query":{"call":<u64>,"execution":"<id>"}}` |
| `heartbeat` | `"heartbeat"` (optional, ignored; the session's `ack` already keeps the link alive) |
| `progress` | `{"progress":{"call":<u64>,"execution":"<id>","committed":<u64>}}`: optional and ignored; the broker paces the agent's streams by what it passes on |

A client's `call` names no lease. On the agent's side of the broker, every `call` also carries `"lease":"<lease>"`, the lease the call belongs to, which the relay agent keeps in the call's record ([RP-14](#rp-14-records-belong-to-their-lease)): `{"call":{"call":<u64>,"execution":"<id>","op":<op>,"lease":"<lease>"}}`. The broker sets it itself and replaces any `lease` a client sends; a call without the field has no lease.

### Broker → client

| Message | JSON |
| --- | --- |
| `ready` | `{"ready":{"protocol":2,"lease":"<machine name>","agent":<agent or null>}}`, the first message of every new session |
| `pending` | `{"pending":{"call":<u64>,"execution":"<id>","code":"agent_not_connected","message":"…","wait_ms":<u64>}}` |
| chunk | binary, a call's output or a read's bytes |
| `end` | as above, after a call's last chunk |
| `result` | `{"result":{"call":<u64>,"execution":"<id>","replayed":<bool>,"outcome":<outcome>}}`, a call's last message |
| `query_result` | `{"query_result":{"call":<u64>,"execution":"<id>","state":<state>}}` |
| `progress` | `{"progress":{"call":<u64>,"execution":"<id>","committed":<u64>}}`: how many bytes of the call's upload the relay agent holds, from its first byte ([RP-15](#rp-15-resumable-streams)) |
| `revoked` | `{"revoked":{"reason":"released"}}`, before the broker closes the session ([RP-10](#rp-10-revocation)) |

`agent` in `ready` is `{"os":"macos","arch":"aarch64","build":"<sha256 of the agent executable>","protocol":2}` when the lease's relay agent has a session, else `null`. A client ignores message names it does not know.

On the agent's side, the broker sends `ready` too, right after the agent's `hello`, with its own `protocol` and `agent` set to `null`; each side speaks the lower of the agent's `hello.protocol` and the broker's. An agent of protocol 1 receives no `progress`, `resume` or `offset`, and the broker reports an upload's progress for it (RP-15).

### Operations (`op`)

| Operation | JSON | Upload | Answer |
| --- | --- | --- | --- |
| exec | `{"exec":{"command":"<shell line>","stdin":<bool>,"timeout_s":<number>}}` | yes: standard input when `stdin` is true, else send zero chunks | output chunks, `end`, `result` with `exit` |
| script | `{"script":{"timeout_s":<number>}}` | yes: the script | output chunks, `end`, `result` with `exit` |
| write | `{"write":{"path":"<path>","name":"<file name or empty>","mode":<integer or null>}}` | yes: the file's bytes | `result` with `written` |
| read | `{"read":{"path":"<path>"}}` | no | the file's bytes as chunks, `end`, `result` with `read` |
| write_tree | `{"write_tree":{"path":"<path>","name":"<folder name or empty>"}}` | yes: the folder's tar stream ([RP-13](#rp-13-folders)) | `result` with `written` |
| read_tree | `{"read_tree":{"path":"<path>"}}` | no | the folder's tar stream as chunks, `end`, `result` with `read` |
| cancel | `{"cancel":{"target":"<execution ID>"}}` | no | `result` with `cancel` |
| status | `"status"` | no | `result` with `status` |
| restart | `"restart"` | no | `result` with `status`, the state before the restart |
| purge | `{"purge":{"lease":"<lease>"}}` | no | `result` with `purged` ([RP-14](#rp-14-records-belong-to-their-lease)) |

- exec runs the command line through the user's shell with `-c`, as `sshd` does, from the home folder, in its own process group, in the desktop session; standard output and standard error are combined into one stream. script runs the uploaded script on standard input of `zsh -s` on macOS or `bash -s` on Linux. A command is killed with its process group, and on Linux with its systemd scope, which also holds the processes that left the group, after `timeout_s` seconds, which must be positive.
- write resolves `path` against the home folder; when `path` names a folder, the file is `name` inside it. The bytes go to a hidden temporary sibling named by the execution ID, `.<name>.relay-agent-<first 16 hex digits of the SHA-256 of the ID>.part`, are checked against the `end`, and are renamed into place. `mode` is an integer file mode, such as `420` for `0644`.
- read resolves `path` against the home folder. A folder is refused with code `is_directory`; read it with read_tree.
- write_tree and read_tree move a folder as one tar stream ([RP-13](#rp-13-folders)).
- restart replaces the relay agent's process with the executable at its path, which may hold a new build (RA-6). It is taken only while no other operation runs or waits for its end, else refused with `busy`; an agent that cannot replace itself refuses it with `unsupported`. Its record keeps the answer, so a repeated ID never restarts twice. The agent's session ends and its next session says hello with the build now running. machine-service sends it; a client has no need to.
- purge removes the relay agent's records of a lease, with the output they keep. Only machine-service sends it, at release; the broker refuses it from a client with `forbidden` ([RP-14](#rp-14-records-belong-to-their-lease)).
- cancel kills the target's process group (and scope) when it runs, discards it when its upload has not ended, and has its own execution ID like any operation.

### Outcomes (`outcome`)

| Outcome | JSON |
| --- | --- |
| exit | `{"exit":{"rc":<int or null>,"signal":<int or null>,"timed_out":<bool>,"cancelled":<bool>,"output":{"size":<u64>,"sha256":"<hex>"}}}` |
| written | `{"written":{"size":<u64>,"sha256":"<hex>"}}` |
| read | `{"read":{"size":<u64>,"sha256":"<hex>"}}` |
| cancel | `{"cancel":{"target":"<id>","found":"signalled" \| "discarded" \| "completed" \| "unknown"}}` |
| purged | `{"purged":{"lease":"<lease>","records":<u64>,"bytes":<u64>}}`: the records removed and the bytes of output that went with them |
| status | `{"status":{"protocol":2,"build":"<hex>","os":"macos" \| "linux","running":["<id>", …],"ownership":"<text>"}}`; `ownership` is `scope` when each command runs in its own systemd scope, else `process-group (<why not scopes>)` |
| failed | `{"failed":{"code":"<code>","message":"<text>","committed":<bool>}}` |
| interrupted | `{"interrupted":{"message":"<text>"}}`: the relay agent stopped while the operation ran; its effect is uncertain and it never runs again. A restarted agent stops what such a command left running, and the message says so |

### Execution states (`state`)

| State | JSON |
| --- | --- |
| completed | `{"completed":{"op":"exec" \| "script" \| "write" \| "read" \| "write_tree" \| "read_tree" \| "cancel" \| "status" \| "restart","outcome":<outcome>}}`; a purge keeps no record, so its ID is never `completed` |
| running | `{"running":{"op":"<op kind>"}}` |
| unknown | `"unknown"` |

## RP-5: A call

- A client numbers its calls and queries with `call`, any positive integer that is unique among the calls still open in this session; counting up from 1 is simplest. Every message the broker sends about a call carries the client's number.
- An uploading operation (exec, script, write, write_tree) sends `call`, then its bytes as chunks of at most 1,044,480 bytes, then `end` with the size and the SHA-256 of all of them. An exec without standard input still sends `end`, with size 0 and the SHA-256 of nothing, `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
- An upload has no size limit. It is a stream: the broker passes its chunks on to the relay agent as they come and keeps none of them, and the client sends at most the upload window, 32 MiB, beyond the relay agent's last `progress` ([RP-15](#rp-15-resumable-streams)).
- The `end` is the call's commit point in the relay agent: the agent checks the whole stream against it, and nothing runs and nothing is renamed into place before the bytes match it.
- The answer to exec, script, read and read_tree is its stream as chunks, then `end` with the size and the SHA-256 of the whole stream, then `result`. A client checks the stream against its `end`. write, cancel and status answer with `result` alone.
- `result` is always the last message about a call; the call number may be reused after it.
- Calls run at the same time, and their messages interleave; the broker keeps each call's messages in order.
- A `call` whose number is 0 or still open is answered `failed`, code `bad_request`, under that number. A `call` with an empty or over-long execution ID is refused the same way; its chunks and `end` are still taken and dropped.

Example: `echo hi` as exec.

```text
client → {"call":{"call":1,"execution":"7d3c…","op":{"exec":{"command":"echo hi","stdin":false,"timeout_s":60}}}}
client → {"end":{"call":1,"execution":"7d3c…","size":0,"sha256":"e3b0c442…b855"}}
broker ← 0x01 00 00 00 00 00 00 00 01 "hi\n"
broker ← {"end":{"call":1,"execution":"7d3c…","size":3,"sha256":"98ea6e4f…"}}
broker ← {"result":{"call":1,"execution":"7d3c…","replayed":false,"outcome":{"exit":{"rc":0,"signal":null,"timed_out":false,"cancelled":false,"output":{"size":3,"sha256":"98ea6e4f…"}}}}}
```

## RP-6: Commit and failure

- `failed` with `"committed": false` means nothing ran and nothing was recorded; the client may send the same execution ID again.
- `failed` with `"committed": true` means the call passed its commit point and then failed, or that the broker cannot tell whether it did; the client never assumes that it did not run.
- A resend of an execution ID never runs it again ([RP-7](#rp-7-execution-ids-records-and-query)), so resending after a loss is always safe; what a resend cannot do is run a second time.

## RP-7: Execution IDs, records and query

- `execution` is the client's execution ID: 1 to 256 bytes, unique per operation the client means to happen once, within the lease. A random UUID is the usual choice. A repeated ID means "the same operation", never "do it again".
- The broker records every execution ID it handed on, with its outcome once it has one, for at least 1 hour. The relay agent records it as well, on disk, until the lease ends ([RP-14](#rp-14-records-belong-to-their-lease)). Within a lease, an ID is never forgotten sooner than 7 days after it completed.
- A `call` with a known execution ID never runs again. The answer has `"replayed": true` and is:
  - for a completed exec or script: its whole output from the first byte, its `end` and its recorded `result`;
  - for a running one: its output so far, then the rest as it comes, then its `end` and `result`;
  - for a read or read_tree: the recorded `end` and `result`, without the bytes, because read bytes are not kept; read again under a new execution ID to have them;
  - for write, write_tree, cancel and status: the recorded `result`.
- The upload of a call with a known execution ID is still sent and is ignored.
- `query` asks what is known of an execution ID without sending it again. The answer is the broker's record when it has one, else the relay agent's: `completed` with its outcome, `running`, or `unknown`. `unknown` means neither has a record: the operation never reached its commit point, or its records expired. A client that had sent the call reports such an operation as uncertain.
- After a lost session, a client recovers each call it had in flight in a new session: an upload whose `end` it had not sent continues with `resume` ([RP-15](#rp-15-resumable-streams)); for any other call it sends `query`, or it sends the same `call` with the same execution ID and `resume` and takes the replayed answer. Both survive a broker restart, because the relay agent keeps its records and its uploads.
- A read cut by a loss continues under a new execution ID with `offset`, the bytes the client already holds ([RP-15](#rp-15-resumable-streams)). A read whose bytes do not match its `end` is read again from the first byte under a new execution ID.

## RP-8: When the relay agent is not connected

- When the lease's relay agent has no session, the broker answers a `call` or `query` with `pending`, code `agent_not_connected`, and keeps it waiting for up to `wait_ms` for the relay agent to register: the agent session's grace period, 10 minutes by default (`MACHINE_SERVICE_RELAY_AGENT_WAIT`). A later caller attached to the same execution receives no second `pending`.
- When the relay agent registers in time, the call proceeds as usual. Otherwise the call ends with `failed`, code `agent_not_connected`, `"committed": false`, and a query with state `unknown`.
- A relay agent whose connection breaks keeps its session for its grace period, and the broker's messages wait in that session's replay buffer, so a short break needs nothing from the client.
- While machine-service restarts a lease's relay agent onto a new build (RA-6), the broker holds new calls back until the agent's next session, as if the agent had no session; such a call may receive `pending`.
- When the relay agent's session ends while a call is out, the broker sends the call again, with the same execution ID, to the agent's next session, and the relay agent answers it from its record. When no next session comes within `wait_ms`, the call ends with `failed`, code `agent_lost`, `"committed": true`: its outcome is uncertain, and `query` later tells it.
- The bytes of an upload that were on their way when the agent's session ended are gone, since the broker keeps none. A client's upload whose `end` had not reached the broker ends at once with `failed`, code `upload_incomplete`, `"committed": false`, and the client continues it with `resume` from the agent's committed offset ([RP-15](#rp-15-resumable-streams)). One whose `end` had is sent again with `resume` and its `end`, without chunks: the relay agent commits it when it holds every byte, answers from its record when it had committed it, and otherwise answers `upload_incomplete`, which the client continues the same way.
- A stream the agent sends is sent again from the byte the client already received, with `offset`, so the client sees no byte twice and no gap.

## RP-9: Error codes

Codes of `failed` outcomes. The broker adds the first group; the relay agent sends the second.

| Code | From | `committed` | Meaning |
| --- | --- | --- | --- |
| `agent_not_connected` | broker | false | the relay agent did not register within `wait_ms`; nothing ran |
| `agent_lost` | broker | true | the relay agent went away after it was handed the call; the outcome is uncertain |
| `bad_request` | broker | false | a malformed call: an empty or long execution ID, a call number in use, an upload for an operation that takes none |
| `checksum_mismatch` | broker or agent | false | the upload does not match its `end`: other bytes, or more than it names; the agent drops what it held of it |
| `upload_incomplete` | broker or agent | false | the agent holds fewer bytes than the `end` names, or its session ended during the upload; it keeps them, and the client continues with `resume` ([RP-15](#rp-15-resumable-streams)) |
| `window_exceeded` | broker | false | the client sent more than the upload window beyond the agent's progress ([RP-15](#rp-15-resumable-streams)) |
| `too_large` | broker | false | an upload of a protocol-1 client exceeds 256 MiB |
| `revoked` | broker | false, or true once handed on | the lease was released while the call waited or ran |
| `invalid_request` | agent | false | a bad execution ID or `timeout_s` |
| `call_in_use`, `superseded`, `end_mismatch`, `cancelled` | agent | false | the agent refused the call before its commit point |
| `stopping`, `record_failed` | agent | false | the agent cannot run it now; the broker sends a call refused with `stopping` again to the agent's next session instead of answering it |
| `forbidden` | broker or agent | false | a purge from a client, or one whose call does not belong to the lease it names ([RP-14](#rp-14-records-belong-to-their-lease)) |
| `busy`, `unsupported` | agent | false | a restart was refused: an operation runs, or the agent cannot replace itself; or a purge was refused because an operation of the lease runs |
| `purge_failed` | agent | true | some of the lease's records could not be removed; a purge may be sent again |
| `not_found`, `is_directory`, `write_failed`, `read_failed` | agent | false before the end, true after | a file operation failed |
| `not_a_directory`, `bad_archive`, `unsupported_entry` | agent | false | a folder operation was refused: the path is a file, the uploaded tar stream is not a folder of files and folders, or the folder to read holds a link or a device ([RP-13](#rp-13-folders)) |

- A body that is not a relay message makes the broker close the session as a protocol error. A chunk or `end` for a call that is not open is dropped: a call may end, with a refusal or `upload_incomplete`, before its stream does.

## RP-10: Revocation

- When a lease starts releasing or expires, the broker refuses its credentials at once: every later `hello`, resumes included, gets `error` with code `unauthorized`.
- Each live client session of the lease first receives `revoked` with the reason, `released` or `expired`, and is then closed with `bye`. Calls still waiting end with `failed`, code `revoked`.
- The relay agent's session is closed the same way. Release then stops the virtual machine, which stops every command in it.

## RP-11: Discovery and acquisition

| Where | Field | Value |
| --- | --- | --- |
| `GET /health` | `relay_url`, `relay_ca_sha256` | the broker's URL and its authority's fingerprint |
| Bonjour TXT of `_machine-service._tcp` | `relay_url`, `relay_ca_sha256` | the same |
| Consul service metadata | `relay_url`, `relay_ca_sha256` | the same, from `machine-service-install catalog-description` |
| `GET /relay/ca.pem` | the body | the authority's certificate, PEM |
| `POST /acquire` answer | `relay` | `{"url":"…","credential":"…","ca_sha256":"…","ca_pem":"…"}`, only when the lease's link is `relay`; a durable machine's acquisition carries it when the instance runs the broker |
| the lease record, and the acquisition's answer | `link` | `relay`, or `ssh-fallback` when the relay agent did not register (RA-8) |
| the same | `link_reason` | why the lease is `ssh-fallback`, such as `the relay agent did not register within 120s`; absent on a `relay` lease |
| the same | `relay_agent` | on a `relay` lease, `{"os","arch","build","broker_url"}`: the relay agent as it registered and the broker URL its guest dials, which may differ from `relay_url` (BK-5) |

- The broker runs when the instance is started with `MACHINE_SERVICE_GUEST_LINK=relay`; otherwise none of these fields appear.
- `relay_url` names the instance's host name (on macOS its Bonjour name, `<LocalHostName>.local`) and the session listener's port, `MACHINE_SERVICE_SESSION_PORT`, 6241 by default. `MACHINE_SERVICE_RELAY_URL` replaces it with a URL that other sites reach, such as a Tunnel host name (BK-4, TC-6).
- The credential appears only in the acquisition's answer; `GET /machines` never shows it. A client that loses it releases the lease and acquires another.
- A lease whose `link` is `ssh-fallback` has no `relay` field; its actions go over machine-service's HTTP API and SSH, outside the desktop session.

## RP-12: Constants

| Constant | Value |
| --- | --- |
| path | `/relay` |
| session protocol `version` | 1 |
| relay `protocol` | 2 |
| largest WebSocket message | 1,048,576 bytes |
| largest envelope body | 1,048,560 bytes |
| largest chunk data | 1,044,480 bytes |
| largest upload | none; 268,435,456 bytes from a protocol-1 client |
| upload window | 33,554,432 bytes (32 MiB) beyond the last `progress` |
| progress step | 4,194,304 bytes (4 MiB): the receiver reports at least this often |
| a held upload is kept | 24 h after its last byte, unless continued, cancelled or purged |
| execution ID | 1 to 256 bytes |
| heartbeat | 5 s by default, at most 30 s |
| dead connection | 15 s without traffic, or 15 s of outstanding bodies without a new acknowledgement |
| reconnect backoff | base 250 ms doubling to 5 s, uniform jitter in the upper half |
| session grace | 10 min by default (`grace_ms`) |
| broker record retention | at least 1 h |
| relay agent record retention | until the lease's purge; at most 7 days after completion |

## RP-13: Folders

A folder travels as one tar stream, uploaded by write_tree and answered by read_tree like a file's bytes: in chunks of at most 1,044,480 bytes, checked by the size and SHA-256 of the whole stream in its `end`, and resumable like any stream ([RP-15](#rp-15-resumable-streams)).

- **The stream** is a POSIX tar archive (ustar headers; a long path in a GNU `L` entry or a PAX `x` header, as common tar libraries write them, such as npm `tar-stream` or Rust `tar`) of the folder's contents, not of the folder itself: entry paths are relative to the folder, such as `a.txt` and `nested/b.bin`, use `/`, and never start with `/` or hold a `..` component. A folder entry is optional when a file inside it names its path.
- **Entries** are regular files (type `0`) and folders (type `5`) only. A file's mode bits are kept; owners and times may be sent and are not relied on. The archive ends with two 512-byte zero blocks.
- **write_tree** resolves `path` against the home folder, as write does; when `path` is an existing folder, the folder lands inside it under `name`, as `scp -r` copies a folder into a folder, else it lands at `path`. The relay agent spools the stream to a hidden sibling of the destination named by the execution ID, `.<name>.relay-agent-<16 hex digits>.tar.part`, checks it against the `end`, and unpacks it into another hidden sibling: nothing is visible before the commit point. A stream that does not match its `end` is refused with `checksum_mismatch`, and one with an absolute path, a `..`, a link or a device with `bad_archive`, both with `"committed": false`. At the commit point the folder is put in place in one step: a rename, or, when a folder is there already, an atomic exchange with it (`renamex_np` with `RENAME_SWAP` on macOS, `renameat2` with `RENAME_EXCHANGE` on Linux), after which the old folder is removed. The folder is replaced whole, not merged: a reader sees the old folder or the new one. A destination that is a file is refused with `not_a_directory`. The `written` outcome carries the stream's size and SHA-256.
- **read_tree** resolves `path` against the home folder and answers its tar stream, in the same form. A file is refused with `not_a_directory`, a missing path with `not_found`, and a folder that holds a symbolic link, a device or a socket with `unsupported_entry`, before any byte is sent. The `read` outcome carries the stream's size and SHA-256.
- Neither stream has a size limit. A read_tree's stream is made in the same order each time, files and folders sorted by name, so a read_tree continues at an `offset` like a read while the folder does not change.
- A client that holds a read_tree's stream unpacks it into a hidden sibling of its own destination and puts it in place the same way. machine-service's `pull` does so, and its `push` of a folder sends write_tree: neither uses SSH on a `relay` lease. When `pull` finds that the guest path is a folder (a read answered `is_directory`), its read_tree takes the execution ID `tree:<SHA-256 of the read's ID>`, so a repeated request derives the same one.

Example: a folder of one file, `a.txt` holding `hi`.

```text
client → {"call":{"call":5,"execution":"9a1f…","op":{"write_tree":{"path":"work","name":"site"}}}}
client → 0x01 00 00 00 00 00 00 00 05 <2,048 bytes: a 512-byte header for "a.txt", 512 bytes of data, two zero blocks>
client → {"end":{"call":5,"execution":"9a1f…","size":2048,"sha256":"<SHA-256 of the 2,048 bytes>"}}
broker ← {"result":{"call":5,"execution":"9a1f…","replayed":false,"outcome":{"written":{"size":2048,"sha256":"…"}}}}
```

## RP-14: Records belong to their lease

A relay agent's records, and the command output that they keep for a replay, belong to the lease whose call made them (DC-173).

- **Tagging.** The broker names the lease on every call it hands to a relay agent (RP-4): a virtual machine's lease by its machine name, a durable machine's lease by its lease ID. The relay agent keeps the name in the call's record.
- **Purge.** At release machine-service has the broker send the relay agent `purge` with the lease, as a call of that lease. The relay agent takes it only when the call's `lease` is the one it names, else `forbidden`; while an operation of the lease runs it answers `busy`, and the broker asks again. It then removes every record of the lease with its output and answers `purged` with the counts. A purge keeps no record of its own, so sending it again is safe and finds nothing more. The broker drops its own records of the lease when the agent confirms.
- **Release.** A durable lease's release waits up to 60 seconds for the confirmation, or the instance's agent wait when that is shorter. A purge that is not confirmed fails the hygiene record's `relay-records` step with the reason, and the Mac is still freed ([DM-7](durable-machine.md#dm-7-release)). The purge stays pending in machine-service's state and is sent again each time the Mac's relay agent registers, until the agent confirms it (DC-177). A lease none of whose calls reached the relay agent is skipped. A virtual machine's release sends no purge, because the records are deleted with the VM; its answer says so in `relay_records`.
- **Safety bound.** A record whose purge never comes, because no instance knows to purge it, is removed 7 days after it completed: for example after machine-service's state was lost, or when the agent is not registered for a week. A record without a lease, from a broker that named none, is removed only by this bound.
- After a purge, `query` of a purged ID answers `unknown`, and a `call` with it runs as a new operation; the lease has ended, so no client of it is left to send one.

## RP-15: Resumable streams

Protocol 2 makes every stream resumable, so an upload or a read of any size survives any loss without a byte sent twice, and the broker holds about one window of a stream, never the stream (DC-1xx-UP-1). The commit point does not move: the relay agent checks the SHA-256 of the whole stream against its `end` and renames into place only then, so a stream is at most once and never visible half written.

### Uploads

- **The agent holds the stream.** The relay agent writes an upload's bytes into a spool named by its execution ID, which outlives the session that carried them: the hidden sibling of RP-4's write, the tar spool of RP-13, or for exec and script a file in the agent's state folder. It keeps a held upload for 24 hours after its last byte, until a call continues it, a `cancel` names it (`found` is `discarded`) or a purge of its lease removes it.
- **Progress.** The relay agent reports how many bytes of the upload it holds, from the stream's first byte, as `progress`: first when it takes the call, then at least every 4 MiB. The broker passes each one to the client under the client's call number.
- **Window.** A client keeps at most 32 MiB of an upload beyond the last `committed` it received for the call; before the first, beyond the byte its chunks started at. A client that sends more is answered `failed`, code `window_exceeded`, and its chunks are dropped. Exactly one `call` of an execution ID feeds its upload at a time: a later `call` with the same ID supersedes the earlier one, which is answered `failed`, code `superseded`.
- **Continuing.** A `call` with `"resume": true` continues the upload the agent holds for its execution ID. Its first answer is `progress` with the agent's committed offset, `0` when the agent holds nothing; the client then sends the stream's bytes from that offset, within the window, and the `end` of the whole stream. A `call` without `resume` starts the upload again from the first byte, and the agent drops what it held. When the execution ID already passed its commit point, the answer is the recorded `result` (RP-7), without a `progress`, and nothing is sent again.
- **When to continue.** A client continues an upload, with the same execution ID, after:
  - `failed` with code `upload_incomplete`: the agent's session ended during the upload, or the `end` named more bytes than the agent holds;
  - a session that cannot resume (RS-5), when the upload's `end` had not been sent; one whose `end` had been sent is recovered as RP-7 says, and continuing it with `resume` is the same as sending it again;
  - a restart of the broker, which ends the client's session.
- **Short and corrupted streams.** An `end` naming more bytes than the agent holds is answered `upload_incomplete`, and the agent keeps the bytes. An `end` naming other bytes (another SHA-256, or fewer bytes than the agent holds) is answered `checksum_mismatch`; the agent drops the upload and records nothing, so the same execution ID may be sent again. Neither reaches the destination.
- **Durability.** The relay agent flushes a spool to the disk at least every 64 MiB, when the session that feeds it ends, and before it stops or restarts. An agent that restarts keeps the bytes it had flushed and continues from there.

### Reads and output

- A `call` with `"offset": <n>` starts the answer's stream at byte `n`: for read and read_tree, and for the output an exec or script replays (RP-7). The `end` still names the size and SHA-256 of the whole stream, which the client checks against the bytes it holds from earlier calls and this one.
- A read cut by a lost session continues with a new execution ID and `offset`, the bytes the client holds; the client keeps hashing from where it stopped. A read whose whole stream does not match its `end`, as when the file changed between the calls, is read again from the first byte.
- The broker passes the relay agent's streams on as the client session takes them, and acknowledges them to the agent with `progress`; the agent sends at most 32 MiB beyond that. A client acknowledges nothing: the session's own flow paces it.

### A client's upload, step by step

```text
size, sha256 = the whole stream's
offset = 0, resume = false
loop:
  send call {call: n, execution, op, resume}
  if resume: wait for the call's first progress -> offset   (a result instead: done)
  for each chunk from offset:
    wait while sent - last progress >= 32 MiB               (a result instead: stop sending)
    send chunk
  send end {size, sha256}
  wait for result:
    failed upload_incomplete -> resume = true, n = a new call number, loop
    anything else            -> done
on a session that cannot resume, before the end went out:
  open a new session, resume = true, loop
```

Example: a 3 MiB write continued after the relay agent's session ended at its first MiB.

```text
client → {"call":{"call":1,"execution":"5b0e…","op":{"write":{"path":"f.bin","name":"","mode":420}}}}
broker ← {"progress":{"call":1,"execution":"5b0e…","committed":0}}
client → 0x01 …01 <1,044,480 bytes>
client → 0x01 …01 <1,044,480 bytes>
broker ← {"result":{"call":1,"execution":"5b0e…","replayed":false,"outcome":{"failed":{"code":"upload_incomplete","message":"…","committed":false}}}}
client → {"call":{"call":2,"execution":"5b0e…","op":{"write":{"path":"f.bin","name":"","mode":420}},"resume":true}}
broker ← {"progress":{"call":2,"execution":"5b0e…","committed":1044480}}
client → 0x01 …02 <the stream's bytes from 1,044,480 on, in chunks>
client → {"end":{"call":2,"execution":"5b0e…","size":3145728,"sha256":"<SHA-256 of all 3 MiB>"}}
broker ← {"progress":{"call":2,"execution":"5b0e…","committed":…}}
broker ← {"result":{"call":2,"execution":"5b0e…","replayed":false,"outcome":{"written":{"size":3145728,"sha256":"…"}}}}
```
