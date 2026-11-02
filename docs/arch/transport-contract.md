# Transport contract (TC)

## Why

- The relay stack is built and tested on an intranet only (DC-101).
- Tailscale and Cloudflare Tunnel must still work, so the design relies only on what every supported network guarantees, and the tests impose the worst case of that guarantee on the intranet.
- Tailscale gives each machine a virtual network interface and carries IP packets over WireGuard, directly or through its DERP relays over HTTPS on port 443, so it carries anything an intranet carries.
- Cloudflare Tunnel carries HTTP and WebSocket traffic through Cloudflare's proxy, which ends TLS at Cloudflare's edge, closes a WebSocket that is idle for 100 seconds on the Free and Pro plans, and may close long WebSocket connections without warning.
- The contract below is the worst case of both networks. A link that meets every rule carries the relay stack, and the [relay acceptance](test-layers.md#tl-7-relay-acceptance) suite proves that the stack relies on nothing more.

## TC-1: One outgoing WebSocket over TLS

- The link carries one kind of connection: an outgoing TCP connection to port 443 or to the broker's port, carrying a WebSocket that is set up with a standard HTTP upgrade and runs over TLS.
- The relay stack uses no UDP, no inbound connection and no other port for actions.

## TC-2: A connection may close at any time

- The link may close any connection at any moment, with or without a TCP reset.
- The relay stack resumes the session on a new connection, and no operation runs twice ([RS-2](remote-session.md#rs-2-session-sequence-and-acknowledgement)).

## TC-3: An idle connection closes after 100 seconds

- The link may close a connection that carries no data for 100 seconds.
- Each side of a session sends a heartbeat at least every 30 seconds.

## TC-4: An address or a path may change

- The client's source address, source port and network path may change between connections.
- A session is identified by its session ID, never by an address or a connection.

## TC-5: TLS may end in the middle

- A proxy on the link may end TLS and open a new TLS connection to the broker.
- Every credential is a bearer token inside the protocol, and no credential depends on a client certificate.

## TC-6: The broker is reached by name

- The link may reach the broker only by a host name, such as a Tunnel hostname or a Tailscale name.
- Discovery advertises a URL, and no client needs the broker's IP address ([BK-4](broker.md#bk-4-discovery-of-the-broker)).

## TC-7: Latency and jitter may be high

- The link may add up to 1 second of round-trip latency, with jitter, and may limit bandwidth.
- Timeouts are sized for that link, and no protocol step depends on a fast round trip.

## TC-8: A message may be limited in size

- A proxy on the link may refuse a WebSocket message larger than 1 MiB.
- Command output, scripts and files travel in chunks of at most 1 MiB.

## What the contract does not cover

- The contract proves that the relay stack works on any link that meets it. It does not prove that a given network meets it.
- An operator checks a network once, when setting it up, by having `machctl` reach the broker through its advertised `relay_url`.
- Cloudflare Tunnel itself prefers QUIC over UDP between `cloudflared` and Cloudflare. A network that blocks or intercepts UDP may force `cloudflared` back to TCP, and that is the operator's setup, outside this contract.
