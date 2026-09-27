---
type: Design
title: event-bus
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

This document set specifies the design of a durable event bus used by the 24/7
agent described in [vision.md](../vision.md).

The bus accepts events over a Unix domain socket (UDS) using a WebSocket
transport, fans them out to subscribers, and persists every event to a
write-once-read-many (WORM) write-ahead log (WAL) so that history survives
crashes and can be replayed.

## Topics

| Document | Topic |
| --- | --- |
| [transport.md](./transport.md) | UDS listener, WebSocket handshake, peer credentials. |
| [protocol.md](./protocol.md) | Wire messages and a worked exchange example. |
| [cloudevents.md](./cloudevents.md) | The event envelope and bus-owned attributes. |
| [wal.md](./wal.md) | WORM WAL storage: records, hash chain, segments, recovery, retention. |
| [delivery.md](./delivery.md) | Broker fan-out, durable cursor, restart semantics, guarantees. |
| [security.md](./security.md) | Access control and tamper evidence. |

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
  [Retention](./wal.md#retention).
- Cross-host transport. Remote clients are supported only through an external
  proxy; see [Transport](./transport.md).

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

## Assumptions

- The bus runs as a single process and is the sole writer to its WAL directory.
- Subscribers tolerate duplicate deliveries.
- Operators accept that the log grows without bound inside the application and
  handle archival at the deployment layer.

Unresolved decisions are tracked in [Open Questions](#open-questions).

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

| Layer | Responsibility | Document |
| --- | --- | --- |
| `transport` | UDS listener, HTTP/1.1 Upgrade, WebSocket framing, peer credential extraction. | [transport.md](./transport.md) |
| `protocol` | Message schemas, subprotocol versioning, request/response correlation. | [protocol.md](./protocol.md) |
| `broker` | Subscriber registry, bounded per-subscriber queues, fan-out, backpressure policy. | [delivery.md](./delivery.md) |
| `wal` | Segmented append-only store, group commit, hash chain, index, recovery. | [wal.md](./wal.md) |
| `cursor` | Durable per-subscriber progress (`ack`). | [delivery.md](./delivery.md) |
| `recovery` | Log scan and verification at startup, replay from cursor. | [delivery.md](./delivery.md) |

The logical event envelope is CloudEvents 1.0. The physical on-disk frame is a
custom WAL record that wraps the CloudEvents JSON. See
[CloudEvents Envelope](./cloudevents.md) and [WAL / WORM Log](./wal.md).

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
7. Observability: structured `tracing` logs, the close-code taxonomy, and a
   `traceparent` that the bus preserves but does not author. An OpenTelemetry
   exporter is deferred until a collector and a sampling policy exist; W3C trace
   context needs no SDK at the bus, only string preservation.

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
