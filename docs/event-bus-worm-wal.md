---
type: Design
title: event-bus-worm-wal
description: A durable event bus over a Unix domain socket WebSocket server, persisted by a WORM WAL
tags:
  - eventbus
  - WAL
  - WORM
  - WebSocket
  - CloudEvents
  - unix-domain-socket
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## About

This document specifies the design of a durable event bus used by the 24/7 agent
described in [vision.md](./vision.md).

The bus accepts events over a Unix domain socket (UDS) using a WebSocket
transport, fans them out to subscribers, and persists every event to a
write-once-read-many (WORM) write-ahead log (WAL) so that history survives
crashes and can be replayed.

## Context and Goals

A continuously running agent cannot rely on in-process channels alone. Work that
survives a restart needs a durable, ordered, replayable record of what happened.
This design provides that record and a live subscription mechanism on top of it.

Goals:

- Accept events from any local process over a UDS.
- Persist every accepted event in a strictly ordered, append-only log.
- Provide tamper evidence: any mutation of persisted bytes is detectable.
- Fan out live events to multiple subscribers.
- Allow subscribers to resume from a durable cursor after a restart.
- Keep the write path non-blocking with respect to slow subscribers.

Non-goals:

- Distributed consensus or multi-writer replication.
- Exactly-once delivery.
- Physical WORM media (optical disc, tape, S3 Object Lock). See
  [Retention](#retention).
- Cross-host transport. Remote clients are supported only through an external
  proxy; see [Transport](#transport-uds--websocket).

## Requirements

Functional:

- `publish`: append an event, durably commit it, and return its sequence
  number.
- `subscribe`: receive events from a given sequence number, optionally filtered.
- `ack`: durably record a subscriber's progress.
- `replay`: re-deliver persisted events from any sequence number.
- `verify`: detect any modification to persisted log bytes.

Non-functional:

- Durability: a `publish` is acknowledged only after the record reaches stable
  storage.
- Ordering: every event has a monotonically increasing sequence number, and
  subscribers observe events in sequence order.
- Non-blocking writes: a slow subscriber must never stall the log writer.
- Local authorization: access is gated by UDS filesystem permissions and peer
  credentials.
- Bounded memory: the broker holds no unbounded per-subscriber buffer.

## Assumptions and Open Questions

Assumptions:

- The bus runs as a single process and is the sole writer to its WAL directory.
- Subscribers tolerate duplicate deliveries.
- Operators accept that the log grows without bound inside the application and
  handle archival at the deployment layer.

Open questions are tracked in [Open Questions](#open-questions).

## Architecture Overview

```
            UDS (pathname socket)
 client ───────────────────────────► ws listener
                                       │  handshake + SO_PEERCRED
                                       ▼
                                     protocol
                                       │  publish / subscribe / ack
                          ┌────────────┴────────────┐
                          ▼                         ▼
                       broker                    cursor store
                          │  fan-out                 ▲ ack
                          ▼                          │
                    ┌─────┴─────┐                    │
                    ▼           ▼                    │
              subscriber   subscriber                │
                                     ▲               │
                                     │ replay        │
                                  WAL (WORM, hash-chained, append-only)
```

| Layer | Responsibility |
| --- | --- |
| `transport` | UDS listener, HTTP/1.1 Upgrade, WebSocket framing, peer credential extraction. |
| `protocol` | Message schemas, subprotocol versioning, request/response correlation. |
| `broker` | Subscriber registry, bounded per-subscriber queues, fan-out, backpressure policy. |
| `wal` | Segmented append-only store, group commit, hash chain, index, recovery. |
| `cursor` | Durable per-subscriber progress (`ack`). |
| `recovery` | Log scan and verification at startup, replay from cursor. |

The logical event envelope is CloudEvents 1.0. The physical on-disk frame is a
custom WAL record that wraps the CloudEvents JSON. See
[CloudEvents Envelope](#cloudevents-envelope) and [WAL / WORM Log](#wal--worm-log).

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
logging.

Because the transport is a UDS, there is no browser `Origin` to validate. A
remote or browser client is out of scope for the bus itself and is expected to
arrive through an external TCP-to-UDS proxy that terminates authentication and
forwards the connection. How the proxy conveys an authenticated identity to the
bus is an [open question](#open-questions). The bus protocol is kept
proxy-compatible (standard HTTP upgrade, standard WebSocket frames) so such a
proxy can be added without changing the bus.

WebSocket-specific handling:

- Client-to-server frames MUST be masked, per RFC 6455.
- Text frames are validated as UTF-8.
- Ping/pong and close frames are handled by the WebSocket layer, not the
  application protocol.

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
| `subscribe` | `from_seq` (integer), optional `filter` | Begin delivery at `from_seq` (or the stored cursor if greater). |
| `ack` | `cursor` (integer) | Durably record delivery progress. |

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

### Example Exchange

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
{"type":"subscribe","from_seq":1000}
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

## CloudEvents Envelope

Persisted and delivered events conform to CloudEvents 1.0. This section fixes
the choices the base specification leaves open.

Required attributes:

- `specversion`: `"1.0"`.
- `type`: a reverse-DNS or URI-style event type, e.g. `agent.task.started`.
- `source`: the bus instance URI, assigned by the bus. See below.
- `id`: unique per `source`. A ULID is recommended so IDs are sortable and
  collision-resistant.

Optional attributes used:

- `time`: RFC 3339 UTC, set by the bus at commit if omitted.
- `subject`: the logical stream a producer wants to address (e.g. a task ID).
- `datacontenttype`: the media type of `data` when present.
- `data`: the event payload.

When `data` carries binary content, the JSON format requires it be encoded as a
base64 string under `data_base64` instead of `data`. The model supports both.

Extensions used:

- `sequence` (CloudEvents Sequence extension): the bus's global sequence number,
  assigned by the bus. See below.

The bus owns three attributes and rewrites them at commit, discarding any
client-supplied values:

- `source` is overwritten with the bus instance URI.
- `sequence` is overwritten with the assigned global sequence, rendered as a
  20-digit zero-padded decimal string (`format!("{:020}")`). A fixed-width
  decimal encoding makes lexicographic order equal numeric order for the full
  `u64` range, satisfying the Sequence extension's ordering requirement.
- `time` is set by the bus if the client omitted it.

The bus also assigns `id` if absent. Rejecting clients that attempt to set
`sequence` or `source` outright is simpler to reason about and is the
recommended behavior; rewriting is the fallback.

Serialization:

The bus implements the CloudEvents JSON format with `serde` directly rather than
through an SDK. This keeps the serialization byte-exact under our control, which
matters because the persisted bytes are hashed and chained. The model is a plain
struct: required fields plus a map of extension attributes and an optional
`data` value.

## WAL / WORM Log

The WAL is the source of truth. It is append-only: once a record is committed it
is never modified or deleted. A record is a CloudEvents JSON envelope wrapped in
a fixed physical frame that provides framing, integrity, and chaining.

### Record Layout

All integers are little-endian.

```
offset  size  field
0       4     magic            = "AEB1" (0x41 0x45 0x42 0x31)
4       1     frame_version    = 1
5       3     reserved         zero
8       8     seq              u64
16      8     timestamp_ns     u64 (bus commit time, nanoseconds since epoch)
24      4     payload_len      u32
28      N     payload          CloudEvents 1.0 JSON, UTF-8
28+N    4     crc32c           over bytes [0, 28+N)
32+N    32    prev_hash        BLAKE3-256 of the previous record's full bytes
```

Total record size is `64 + N` bytes. The frame is self-delimiting: a reader
computes the end of a record from `payload_len` without parsing JSON, which
keeps the recovery hot path free of a streaming JSON parser.

Every record in the log is an event. There is no record discriminator: cursors
and any future snapshot live in separate files, not in the WAL. `frame_version`
is the only evolution hook, and `reserved` bytes MUST be zero and are ignored on
read.

### Hash Chain

Let `R_n` be the full bytes of record `n`, including its trailing `prev_hash`.
Define:

```
h_0        = BLAKE3("agent-eventbus-wal-genesis-v1")     (32 bytes, domain separated)
h_n        = BLAKE3(R_n)                                   for n >= 1
prev_hash(R_n) = h_{n-1}
```

The first record in the log sets `prev_hash = h_0`. Each subsequent record
stores the hash of the immediately preceding record's full bytes. The chain
spans segment boundaries: the first record of a new segment carries the hash of
the last record of the previous segment. Because `prev_hash` is inside the
hashed region, modifying any field of any record breaks every hash after it.
`crc32c` catches accidental corruption cheaply; the chain catches deliberate
mutation.

Verification is a linear pass from the first record: recompute `crc32c`, recompute
`h_n`, and compare it to the `prev_hash` of the next record. A standalone
`verify` tool performs this pass over a directory and reports the first
mismatch. This is the entire tamper-evidence story; see
[Security Model](#security-model) for what it does and does not cover.

### Segments

The log is split into segment files so rotation is cheap and reads can be
memory-mapped.

- Naming: `segment-{first_seq:020}.log`.
- Target segment size: fixed (for example 64 MiB), rolled when the next record
  would exceed it.
- Only the last segment is open for append.
- Rotation is not deletion. Every segment is retained.

A sparse index, `segment-{first_seq:020}.idx`, stores `(seq, file_offset)` every
`k` records (for example `k = 4096`). The index is memory-mapped for reads and
rebuilt by scanning if absent.

### Write Path and Group Commit

A single writer task owns the open segment. Producers send records to it; it
assigns `seq`, computes `crc32c` and `prev_hash`, and appends. Committing is
batched:

1. Collect pending records.
2. `seq`, `crc32c`, and `prev_hash` are assigned in order.
3. Write the batch (one `writev`).
4. `fsync` the segment once for the whole batch.
5. After `fsync` returns, publish the committed range to the broker.

`published{seq}` is sent only after step 4. This is what makes `publish`
durable. The writer never waits on a subscriber; see
[Broker / Fan-out](#broker--fan-out).

### Crash Recovery

On startup:

1. Scan the last segment from the last valid index entry.
2. Validate each record's `crc32c` and chain continuity.
3. A torn or partial trailing record (for example, a crash mid-write) is
   truncated. This is the only place the log is shortened, and it removes only
   bytes that were never acknowledged as committed.
4. A hash mismatch in a record that was not the trailing partial record is a
   fatal integrity error; the bus refuses to start rather than trust the log.

## Broker / Fan-out

The broker receives committed ranges from the writer and delivers them.

- Each subscriber has a bounded queue (for example, a `tokio::sync::mpsc` with a
  fixed capacity).
- Fan-out to a subscriber is never awaited in a way that blocks the writer. If a
  queue is full, the subscriber is treated as slow.

Backpressure policy:

- Default: evict the slow subscriber by closing its connection with a defined
  close code. Because the cursor is durable, the client reconnects and resumes
  from its last `ack`. The lost range is then replayed from the log. This keeps
  memory bounded and avoids a silent-drop mode.
- The `gap` message is reserved for the case where the requested range is no
  longer available locally (for example, once archival moves older segments
  off the hot path). It is not used for ordinary backpressure.

Publishing is acknowledged only after the writer's `fsync`, never after
fan-out. A subscriber can therefore be behind the committed head without
affecting durability.

## Durable Cursor

Each subscriber has a stable identity and a durable cursor.

- Identity: a client-supplied stable subscriber ID, namespaced by peer UID where
  available.
- Storage: `cursors/{subscriber_id}.cursor`, an atomically replaced file
  (write to a temp file, `fsync`, `rename`, `fsync` the directory).
- On `subscribe`, the server uses `max(requested_from_seq, stored_cursor)` as the
  start point, so a resume never silently skips events.
- On `ack`, the server persists the cursor and replies `cursor_ack`.

The cursor file records the last sequence number that the client has confirmed
processing.

## Recovery and Restart Semantics

On restart, the bus:

1. Verifies the log (see [Crash Recovery](#crash-recovery)).
2. Reconstructs the broker head at the last committed `seq`.
3. Loads each subscriber's cursor when it reconnects.

Delivery guarantees:

- At-least-once. A `publish` returns only after durable commit; a subscriber
  resumes from its cursor, which may cause the last batch to be redelivered if
  the crash occurred between delivery and `ack`.
- Ordering is per the global `seq`. Subscribers observe increasing `seq`.
- Duplicates are possible and expected; consumers should be idempotent, for
  example by deduplicating on the CloudEvents `id` or an `idempotency_key`.

## Retention

The application does not delete or compact the log. This is the meaning of WORM
here: append-only at the application layer, with integrity enforced by the hash
chain. WORM applies to the log. Cursor files are mutable by design.

Consequences and how they are handled:

- The log grows without bound. Rotation into new segments is not deletion; all
  segments are kept.
- Bounding disk usage is a deployment concern. Archival or migration of older
  segments to colder storage, and any eventual removal from the hot path, happen
  outside the bus. When a requested range has been archived away, the bus
  answers with `gap` and the client is expected to fall back to the archive.
- If the filesystem fills, the bus must fail `publish` with a clear error rather
  than drop events silently. A degraded read-only mode is a possible follow-up.

The WAL is never rewritten to add a snapshot. If fast recovery becomes
necessary, a snapshot may be added as a separate sidecar file that only
short-circuits replay; it does not modify or replace WAL bytes.

## Security Model

- Access control is the UDS pathname permissions (`0700` directory, `0600`
  socket) plus a `SO_PEERCRED` UID/GID allowlist. There is no authentication
  beyond local peer identity.
- The hash chain provides tamper evidence, not tamper prevention. An attacker
  with write access to the log files can rewrite a suffix of the log, recompute
  the chain, and produce a self-consistent forgery. Detecting that requires an
  external anchor, such as periodically signing the current chain head or
  shipping heads to an append-only external store. That anchor is out of scope
  and is listed in [Open Questions](#open-questions).
- Physical WORM media (S3 Object Lock, tape) would strengthen the guarantee and
  can be layered on at the deployment level; it is not required by this design.

## Failure Modes and Guarantees

| Failure | Behavior |
| --- | --- |
| Crash mid-write | Torn trailing record truncated on recovery; no acked event lost. |
| Crash after `fsync`, before fan-out | Event is durable and replayed to subscribers on reconnect. |
| Slow subscriber | Evicted; resumes from durable cursor on reconnect. |
| Single-record byte flip | `crc32c` mismatch, detected during recovery or `verify`. |
| Deliberate mutation | Hash chain mismatch, detected during recovery or `verify`. |
| Disk full | `publish` fails with an error; no silent drop. |
| Duplicate delivery | Possible by design; consumers deduplicate on `id`. |

| Property | Guarantee |
| --- | --- |
| Durability of acked publish | Yes, after `fsync`. |
| Total order | Yes, by global `seq`. |
| Delivery | At-least-once. |
| Tamper evidence | Yes, subject to an external anchor for suffix rewrites. |

## Proposed Module Layout

A single crate is used initially rather than several, since the boundaries are
not yet proven. Modules mirror the layers above:

```
src/
  lib.rs
  transport/      UDS listener, upgrade handling, WebSocket framing
  protocol/       message types, subprotocol versioning, error codes
  cloudevent/     CloudEvents 1.0 model and JSON serialization
  wal/            record frame, segments, hash chain, index, recovery
  broker/         subscriber registry, bounded queues, fan-out
  cursor/         durable cursor store
  recovery/       startup scan and replay orchestration
bins/
  verify          offline hash-chain verification over a log directory
```

Splitting into separate crates is deferred until a boundary proves stable and
reuse is real.

## Phased Implementation Milestones

1. WAL core: record frame, `crc32c`, BLAKE3 chain, group commit, recovery, and a
   `verify` tool. Tested without any network code.
2. CloudEvents model: `serde` round-trip of the 1.0 JSON format, including the
   `sequence` encoding.
3. Broker: in-process publish/subscribe over a bounded channel, with eviction.
4. Transport: UDS listener, HTTP upgrade, WebSocket framing, `SO_PEERCRED`
   authorization.
5. Protocol and cursor: `publish`/`subscribe`/`ack`, durable cursor, resume.
6. Replay: `gap` handling and archival-boundary behavior.
7. Observability: log metrics, verification reporting, close-code taxonomy.

## Open Questions

- How should an external proxy convey an authenticated remote identity to the
  bus, given that `SO_PEERCRED` reflects the proxy?
- What is the subscriber identity model (`source`/namespace/ACL), and how are
  cursors isolated between subscribers of the same UID?
- What is the archival interface, and who owns the external anchor for the hash
  chain head?
- Should an optional snapshot sidecar be defined for faster cold replay, and
  what is its format?
- Is an idempotency-key based deduplication window worth adding, or is
  consumer-side deduplication sufficient for the intended consumers?
- What is the final backpressure policy if eviction proves too disruptive for
  long-running internal consumers?
