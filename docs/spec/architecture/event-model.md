---
type: Data Model
title: Event model
description: One CloudEvents 1.0 structured-mode message per fact, four extension attributes, and a closed event taxonomy with additive-only payload rules.
tags: [events, cloudevents, schema]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
sources:
  - id: cloudevents-1-0
    resource: https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md
    title: CloudEvents specification, version 1.0.2
    author: process:cncf-cloudevents
---

# The envelope

Every event is one CloudEvents 1.0 message in structured mode with `datacontenttype: application/json`. The wire format on the socket, the export format, and the format inside a segment are the same bytes. One representation, no translation layer.[^cloudevents-1-0]

| Attribute | Value | Notes |
|---|---|---|
| `specversion` | `1.0` | Constant. |
| `id` | 64 lowercase hex characters | Content hash, computed as below. |
| `source` | `agent://mokmok` | Instance identity lives in the store path, not the source URI. |
| `type` | Reverse-DNS string | See the taxonomy. |
| `subject` | `branch/<id>`, `task/<id>`, or `instance/<id>` | The aggregate the fact belongs to. |
| `time` | RFC 3339 UTC, second precision | When the fact was decided, not when it was written. |
| `datacontenttype` | `application/json` | Constant. |
| `data` | Object | Type-specific payload. |

# Extension attributes

CloudEvents allows lower-case alphanumeric extension names of at most 20 characters. These nine are ours.[^cloudevents-1-0]

| Extension | Type | Meaning |
|---|---|---|
| `branch` | string | Branch the event belongs to, such as `main`. |
| `prev` | string or null | `id` of the predecessor. Null only for the genesis event. |
| `depth` | integer | Ancestor count, genesis at 0. Must equal predecessor depth plus one. |
| `actor` | string | `human:<id>`, `process:<id>`, or `agent:<model-id>`. |
| `causationid` | string | `id` of the event that caused this one. |
| `correlationid` | string | Shared by every event in one turn. |
| `model` | string | Provider model identifier, present on model events. |
| `schemaver` | integer | Payload schema version for this `type`. Starts at 1. |
| `dataref` | string | Blob path when the payload exceeds the inline limit. |

`traceparent` and `tracestate` from the CloudEvents distributed-tracing extension pass through unchanged when a surface starts a command. They are diagnostic only. No fold reads them.[^cloudevents-1-0]

Extension names are not namespaced by the specification, so a collision with another producer is possible in principle. One producer writes this log, and the taxonomy table owns the names, which makes the collision a code review problem rather than a runtime one.

# Identity is the hash

`id` is the BLAKE3-256 hash of the event's canonical JSON, with the `id` attribute removed, encoded as lowercase hex.[^cloudevents-1-0]

Three properties follow.

- Tamper detection. Recompute the hash from a segment's bytes and compare. A flipped bit fails the check, and so does a rewritten attribute.
- Chain integrity. `prev` is inside the hashed bytes, so the ancestors are committed to. A branch is a hash chain, not a pointer list.
- Free idempotency. Retrying an append of identical bytes produces an identical `id`, so a duplicate is detectable without a sequence counter.

Canonicalization follows RFC 8785 (JSON Canonicalization Scheme). Keys are sorted, numbers use the shortest round-trip form, and no insignificant whitespace survives. Two processes that build the same event from the same inputs produce the same bytes.

A hash collision between two events would require identical content at the same `prev` and the same `time`. That is the same fact recorded twice, and deduplicating it is correct.

# The taxonomy

Type strings are fixed strings, not derived from Rust names, and a table in `agent-core` is the single source of truth.

| `type` | `subject` | Payload |
|---|---|---|
| `dev.mokmok.agent.instance.started` | instance | Version, store schema version, recovered byte count. |
| `dev.mokmok.agent.instance.stopping` | instance | Reason, signal, drain outcome. |
| `dev.mokmok.agent.instance.recovered` | instance | Torn bytes dropped, segments verified, chains rechecked. |
| `dev.mokmok.agent.instance.clock_skew` | instance | Measured wall clock delta and direction. |
| `dev.mokmok.agent.branch.created` | branch | Branch id, parent branch, fork event id, label. |
| `dev.mokmok.agent.branch.labeled` | branch | Branch id, new label. |
| `dev.mokmok.agent.branch.archived` | branch | Branch id, reason. |
| `dev.mokmok.agent.command.received` | branch | Command name, typed arguments, originating surface, `command_id`. |
| `dev.mokmok.agent.turn.requested` | branch | Turn id, input message, model policy, budget. |
| `dev.mokmok.agent.model.requested` | branch | Turn id, prefix chain hash, segment count, prompt tokens, purpose of `turn` or `warm`. |
| `dev.mokmok.agent.model.responded` | branch | Turn id, output, finish reason, usage, provider request id. |
| `dev.mokmok.agent.model.cache.observed` | branch | Turn id, prefix hash and length, reported flag, cached and uncached tokens, first token and total milliseconds. |
| `dev.mokmok.agent.model.failed` | branch | Turn id, error class, retryable flag, attempt, partial flag. |
| `dev.mokmok.agent.message.appended` | branch | Role, content, tool call ids, token count. |
| `dev.mokmok.agent.tool.intent` | branch | Call id, tool name, arguments, idempotency key. |
| `dev.mokmok.agent.tool.result` | branch | Call id, exit status, stdout ref, stderr ref, duration. |
| `dev.mokmok.agent.tool.failed` | branch | Call id, error class, whether the effect may have happened. |
| `dev.mokmok.agent.approval.requested` | branch | Approval id, action summary, risk class, expiry. |
| `dev.mokmok.agent.approval.decided` | branch | Approval id, decision, decider actor, note. |
| `dev.mokmok.agent.schedule.armed` | branch or instance | Timer id, due instant, purpose. |
| `dev.mokmok.agent.schedule.fired` | branch or instance | Timer id, fired instant, lateness. |
| `dev.mokmok.agent.task.created` | task | Task id, title, owning branch, parent task. |
| `dev.mokmok.agent.task.state` | task | From state, to state, reason. |

Adding a case to the taxonomy is a code change to the table plus a test that folds it. There is no runtime registration and no plugin-supplied type.

# Payload rules

- A payload change must be additive: new optional fields, never removed fields and never changed types. `schemaver` stays the same.
- A breaking payload change needs a new `type` string, such as `...model.responded.v2`. The old type keeps folding for existing history.
- Readers must tolerate unknown fields. Writers must not emit a field the type does not declare.
- A reader that meets an unknown `type` folds it as an opaque record. It keeps the raw bytes for export, ignores it for state, and never fails the fold.

# Size and offloading

An event's canonical JSON must stay under 128 KiB. This keeps a segment's frame table small enough that replay is not dominated by giant payloads.

A larger payload is written to a blob file named by its BLAKE3 hash under `blobs/`, and the event carries `dataref` with the relative path plus the byte length and hash. The blob write completes before the event append, so an event never points at a blob that is not on disk. A blob with no referencing event after recovery is garbage and is deleted by `agentctl gc`.

# Ordering

Three orders exist and only one of them is authoritative.

- Physical order is `(segment, offset)`. It is the append order and the only total order.
- Logical order within a branch is `depth`. It is derived from `prev` and is what the reactor reasons about.
- `time` is metadata for humans and for cache expiry estimates. Never sort by `time`.

CloudEvents defines no delivery order, and its `sequence` extension is a per-source hint whose semantics the specification deliberately leaves undefined.[^cloudevents-1-0] Neither can order history, and a broker that reorders would break it. The envelope carries the fact. The store carries the order. That is why `depth` is a hash-committed field rather than a counter a broker could reset.

# An example

```json
{
  "specversion": "1.0",
  "id": "9f2c0d1e7b4a56c8e3f1a2b4c6d8e0f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4",
  "source": "agent://mokmok",
  "type": "dev.mokmok.agent.tool.intent",
  "subject": "branch/main",
  "time": "2026-10-25T12:00:03Z",
  "datacontenttype": "application/json",
  "branch": "main",
  "prev": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809",
  "depth": 481,
  "actor": "agent:deepseek-v4.1-flash",
  "causationid": "5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b",
  "correlationid": "turn-01JA3QM7X2",
  "model": "deepseek-v4.1-flash",
  "schemaver": 1,
  "data": {
    "call_id": "call-01JA3QM8Z4",
    "tool": "shell",
    "arguments": { "program": "/run/current-system/sw/bin/git", "argv": ["status", "--short"] },
    "idempotency_key": "sha256:1f0c..."
  }
}
```

[^cloudevents-1-0]: CloudEvents specification, version 1.0.2
