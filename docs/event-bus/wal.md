---
type: Design
title: wal
description: WORM WAL storage for the event bus: records, hash chain, segments, write path, recovery, retention
tags:
  - eventbus
  - WAL
  - WORM
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## WAL / WORM Log

The WAL is the source of truth. It is append-only: once a record is committed it
is never modified or deleted. A record is a CloudEvents JSON envelope (see
[cloudevents.md](./cloudevents.md)) wrapped in a fixed physical frame that
provides framing, integrity, and chaining.

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
[security.md](./security.md) for what it does and does not cover.

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
[delivery.md](./delivery.md).

### Crash Recovery

On startup:

1. Scan the last segment from the last valid index entry.
2. Validate each record's `crc32c` and chain continuity.
3. A torn or partial trailing record (for example, a crash mid-write) is
   truncated. This is the only place the log is shortened, and it removes only
   bytes that were never acknowledged as committed.
4. A hash mismatch in a record that was not the trailing partial record is a
   fatal integrity error; the bus refuses to start rather than trust the log.

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
