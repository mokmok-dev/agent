---
type: Design
title: cloudevents
description: CloudEvents 1.0 envelope and bus-owned attributes for the event bus
tags:
  - eventbus
  - CloudEvents
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

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

The Sequence extension scopes `sequence` to the event's `source`. Because the
bus fixes `source` to a single bus instance URI for every event it persists,
the bus's global sequence is that source's sequence, and the global total order
is spec-compliant. A producer that wants its own independent sequence conveys
it through a separate extension attribute or through `subject`, not through
`sequence`.

## Serialization

The bus implements the CloudEvents JSON format with `serde` directly rather than
through an SDK. This keeps the serialization byte-exact under our control, which
matters because the persisted bytes are hashed and chained. The model is a plain
struct: required fields plus a map of extension attributes and an optional
`data` value.
