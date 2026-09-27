---
type: Design
title: delivery
description: Broker fan-out, durable cursor, restart semantics, and guarantees for the event bus
tags:
  - eventbus
  - broker
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Broker / Fan-out

The broker receives committed ranges from the writer (see
[wal.md](./wal.md#write-path-and-group-commit)) and delivers them.

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

1. Verifies the log (see [Crash Recovery](./wal.md#crash-recovery)).
2. Reconstructs the broker head at the last committed `seq`.
3. Loads each subscriber's cursor when it reconnects.

Delivery guarantees:

- At-least-once. A `publish` returns only after durable commit; a subscriber
  resumes from its cursor, which may cause the last batch to be redelivered if
  the crash occurred between delivery and `ack`.
- Ordering is per the global `seq`. Subscribers observe increasing `seq`.
- Duplicates are possible and expected; consumers should be idempotent, for
  example by deduplicating on the CloudEvents `id` or an `idempotency_key`.

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
