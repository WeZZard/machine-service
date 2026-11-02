"""The TL-7 network rig's edge: mitmproxy behaving like a Cloudflare edge.

The rig starts mitmdump in reverse mode with this script. The script

- ends TLS from the client and opens a new TLS connection to the broker
  (mitmproxy does both; the broker's certificate is verified against the CA
  the rig passes in `ssl_verify_upstream_trusted_ca`),
- routes by name only: a request whose TLS server name or `Host` is not one of
  `edge_routes` is answered `421` and never reaches a broker (TC-6),
- closes a connection that carries no data for `edge_idle_close_s` seconds,
  counting every byte in either direction, WebSocket pings included (TC-3),
- closes a WebSocket that sends a message larger than `edge_max_message_bytes`,
  with close code 1009, and never forwards that message (TC-8).

It is written against one mitmproxy version, pinned by the rig, and refuses to
load under any other, because the message limit wraps a method of mitmproxy's
WebSocket layer.
"""

import logging
import time

import wsproto.connection
import wsproto.events
from mitmproxy import ctx
from mitmproxy import http
from mitmproxy import version
from mitmproxy.proxy import commands
from mitmproxy.proxy import events
from mitmproxy.proxy.layers import websocket as websocket_layer

PINNED_VERSION = "12.3.1"
TOO_BIG = "edge_message_too_big"
# A timeout far beyond any test run, for an idle close that is turned off.
NO_IDLE_CLOSE_S = 10**8

if version.VERSION != PINNED_VERSION:
    raise RuntimeError(
        f"edge.py is written for mitmproxy {PINNED_VERSION}, not {version.VERSION}"
    )


def parse_routes(text):
    """Parse `name=host:port,name=host:port` into {name: (host, port)}."""
    routes = {}
    for item in filter(None, (part.strip() for part in text.split(","))):
        name, _, target = item.partition("=")
        host, _, port = target.rpartition(":")
        if not name or not host or not port.isdigit():
            raise ValueError(f"edge_routes: bad route {item!r}")
        routes[name.lower()] = (host, int(port))
    return routes


def host_name(authority):
    """The host part of a Host header, lowercased."""
    if authority.startswith("["):
        return authority[1 : authority.find("]")].lower()
    return authority.rsplit(":", 1)[0].lower() if ":" in authority else authority.lower()


class Edge:
    def __init__(self):
        self.routes = {}
        self.max_message_bytes = 0

    def load(self, loader):
        loader.add_option(
            "edge_routes", str, "", "Names the edge serves: name=host:port,..."
        )
        loader.add_option(
            "edge_idle_close_s",
            int,
            100,
            "Close a connection idle for this many seconds; 0 turns it off.",
        )
        loader.add_option(
            "edge_max_message_bytes",
            int,
            1048576,
            "Close a WebSocket that sends a larger message; 0 turns it off.",
        )

    def configure(self, updated):
        if "edge_routes" in updated:
            self.routes = parse_routes(ctx.options.edge_routes)
        if "edge_idle_close_s" in updated:
            idle = ctx.options.edge_idle_close_s
            # mitmproxy's own inactivity watchdog sees every byte of a
            # connection in either direction, which is the edge's idle rule.
            ctx.options.update(tcp_timeout=idle if idle > 0 else NO_IDLE_CLOSE_S)
        if "edge_max_message_bytes" in updated:
            self.max_message_bytes = ctx.options.edge_max_message_bytes

    def requestheaders(self, flow: http.HTTPFlow):
        sni = (flow.client_conn.sni or "").lower()
        name = host_name(flow.request.host_header or "")
        if sni not in self.routes or name != sni:
            flow.response = http.Response.make(
                421, b"edge: unknown host name\n", {"Content-Type": "text/plain"}
            )
            return
        flow.request.host = sni
        flow.request.port = self.routes[sni][1]

    def server_connect(self, data):
        name = str(data.server.address[0]).lower()
        if name in self.routes:
            data.server.address = self.routes[name]
        else:
            data.server.error = f"edge: no route for {name}"

    def websocket_message(self, flow: http.HTTPFlow):
        message = flow.websocket.messages[-1]
        if self.max_message_bytes and len(message.content) > self.max_message_bytes:
            message.drop()
            flow.metadata[TOO_BIG] = True


EDGE = Edge()
addons = [EDGE]


def _pending_bytes(ws):
    return sum(len(part) for part in ws.frame_buf)


def _close_too_big(layer):
    """Close both sides as the edge does for an oversized message."""
    reason = "message too big"
    for ws in (layer.client_ws, layer.server_ws):
        if ws.state in {
            wsproto.connection.ConnectionState.OPEN,
            wsproto.connection.ConnectionState.REMOTE_CLOSING,
        }:
            yield ws.send2(wsproto.events.CloseConnection(code=1009, reason=reason))
        yield commands.CloseConnection(ws.conn)
    layer.flow.websocket.timestamp_end = time.time()
    layer.flow.websocket.close_code = 1009
    layer.flow.websocket.close_reason = reason
    yield websocket_layer.WebsocketEndHook(layer.flow)
    layer.flow.live = False
    layer._handle_event = layer.done


_relay_messages = websocket_layer.WebsocketLayer.relay_messages


def relay_messages(self, event):
    yield from _relay_messages(self, event)
    if self._handle_event != self.relay_messages:
        return
    limit = EDGE.max_message_bytes
    # A finished message over the limit was dropped by the hook; an unfinished
    # one is refused as soon as its buffered part passes the limit.
    oversized = self.flow.metadata.pop(TOO_BIG, False) or (
        limit
        and isinstance(event, events.DataReceived)
        and max(_pending_bytes(self.client_ws), _pending_bytes(self.server_ws)) > limit
    )
    if oversized:
        logging.info(f"edge: closing {self.flow.id}, a message is over {limit} bytes")
        yield from _close_too_big(self)


websocket_layer.WebsocketLayer.relay_messages = relay_messages
