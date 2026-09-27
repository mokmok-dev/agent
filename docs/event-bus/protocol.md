---
type: Design
title: protocol
description: Wire protocol and message schemas for the event bus
tags:
  - eventbus
  - WebSocket
  - protocol
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Wire Protocol

The client and server negotiate a subprotocol during the handshake:

```
Sec-WebSocket-Protocol: agent.eventbus.v1
```

The name `agent.eventbus.v1` is deliberate. The CloudEvents WebSockets protocol
binding is still a draft (`1.0.3-wip`) and, more importantly, it defines only
how to send events; it does not define `publish`, `subscribe`, or `ack`. The bus
therefore does not claim `cloudevents.json`. It uses its own subprotocol and
uses CloudEvents purely as the event envelope format.

All application messages are WebSocket text frames containing JSON. A message is
classified by whether its top-level object carries a `specversion` field:

- A message with `specversion` is a CloudEvents envelope (an event).
- A message without `specversion` is a control message.

Control messages, client to server:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `publish` | `id`, `event` (a CloudEvent), optional `idempotency_key` | Append the event to the log. |
| `subscribe` | `subscriber_id` (string), `from_seq` (integer), optional `filter` | Begin delivery at `from_seq` (or the stored cursor if greater). |
| `ack` | `cursor` (integer) | Durably record delivery progress. |

The `subscriber_id` on `subscribe` is the stable identity the durable cursor is
keyed on, namespaced by peer UID (see
[delivery.md](./delivery.md#durable-cursor)). It is where a client declares who
it is; a later `ack` on the same connection refers to it. An earlier draft of
this table omitted it, which left the cursor with nothing to key on.

Control messages, server to client:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `published` | `id`, `seq` | The event was committed at `seq`. |
| `subscribed` | `from_seq` | Subscription accepted; delivery starts at `from_seq`. |
| `cursor_ack` | `cursor` | The cursor was durably recorded. |
| `gap` | `from`, `to` | The requested range is no longer available locally. |
| `error` | `code`, `message`, optional `id` | A request failed. |

Events are delivered as raw CloudEvents envelopes. The delivery sequence is
carried by the CloudEvents `sequence` attribute, not by a wrapper, so that an
event on the wire is a valid CloudEvent. Control messages carry sequences as
JSON integers; the equivalent CloudEvents `sequence` is that integer's fixed-width
decimal string.

## Example Exchange

Client publishes:

```json
{"type":"publish","id":"req-1","event":{
  "specversion":"1.0",
  "type":"agent.task.started",
  "source":"agent://eventbus",
  "id":"01J8Z...",
  "time":"2026-09-27T00:00:00Z",
  "data":{"task_id":"t-1"}
}}
```

Server commits and replies:

```json
{"type":"published","id":"req-1","seq":1024}
```

Client subscribes:

```json
{"type":"subscribe","subscriber_id":"audit-log","from_seq":1000}
```

Server replays and streams events, then live events:

```json
{"specversion":"1.0","type":"agent.task.started","source":"agent://eventbus",
 "id":"01J8Z...","time":"2026-09-27T00:00:00Z","sequence":"00000000000000001024",
 "data":{"task_id":"t-1"}}
```

Client acknowledges:

```json
{"type":"ack","cursor":1024}
```
