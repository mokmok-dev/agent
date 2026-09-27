---
type: Design
title: transport
description: UDS listener, WebSocket handshake, and peer credential extraction for the event bus
tags:
  - eventbus
  - WebSocket
  - unix-domain-socket
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Transport (UDS + WebSocket)

The bus listens on a pathname UDS, not an abstract socket, so that filesystem
permissions apply.

- The parent directory is created with mode `0700` and owned by the bus user.
- The socket file is created with mode `0600`.
- The listener is removed and recreated on startup; a stale socket is detected
  by attempting a connection first.

The HTTP/1.1 opening handshake and WebSocket framing follow RFC 6455 and run
directly over the UDS stream. On connect, the server reads peer credentials on
Linux via `SO_PEERCRED` (`getsockopt`), yielding the peer PID, UID, and GID.
The bus consults an allowlist keyed on UID/GID; the PID is available for
logging. See [security.md](./security.md) for the access-control model.

Because the transport is a UDS, there is no browser `Origin` to validate. A
remote or browser client is out of scope for the bus itself and is expected to
arrive through an external TCP-to-UDS proxy that terminates authentication and
forwards the connection. How the proxy conveys an authenticated identity to the
bus is an [open question](./README.md#open-questions). The bus protocol is kept
proxy-compatible (standard HTTP upgrade, standard WebSocket frames) so such a
proxy can be added without changing the bus.

WebSocket-specific handling:

- Client-to-server frames MUST be masked, per RFC 6455.
- Text frames are validated as UTF-8.
- Ping/pong and close frames are handled by the WebSocket layer, not the
  application protocol.
