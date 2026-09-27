---
type: Design
title: permissions
description: Runtime egress permission changes — a mutable allowlist and per-connection approval enforced by the proxy, correlated over the event bus
tags:
  - sandbox
  - permission
  - egress
  - approval
  - eventbus
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Runtime Egress Permissions

The requirement this design exists for: **the set of hosts a running sandbox may
reach must be changeable while it runs.** Kernel network policy cannot express
this. Seatbelt freezes a profile at `exec`; Landlock is per-port with no host
dimension; bubblewrap has no host filter at all. See
[network.md](./network.md).

The design separates *enforcement* from *decision*:

- The **kernel** enforces that the egress proxy is the only route out. It is set
  once, at spawn, and never changes.
- The **proxy** decides which destinations are reachable. It is consulted per
  connection and holds mutable state.

Because the destination decision lives in the proxy and not in the kernel, it can
change between two connections of the same running command. Runtime permission
changes are therefore not a kernel operation at all; they are writes to the
proxy's destination set.

There are two ways the set changes:

1. **The mutable allowlist.** An authority adds or revokes a `host:port` rule at
   any time, and it takes effect for subsequent connections immediately.
2. **Per-connection approval.** A destination not on the allowlist is put to an
   approver, who grants, denies, or lets it time out.

With no approver configured, this reduces to "an unlisted destination is
denied", which avoids prompt fatigue.

## The Mutable Allowlist

The allowlist is a set of destination rules the proxy owns per sandboxed
process. A rule is added or revoked by publishing a control event on the
[event bus](../event-bus/README.md); the proxy subscribes and applies it.

The allowlist is implemented as `egress::Allowlist`: one `Mutex` guards both the
rules and the registry of currently-open tunnels, so a revoke removes the rule and
closes the tunnels it granted in one critical section. The closers are invoked
**after** the guard is released, so a closer cannot deadlock against the
allowlist; a poisoned lock denies. The pure decision logic and the revoke policy
are unit-tested with counter closers, and the proxy integration test drives a real
tunnel.

| Direction | Event | Payload | Effect |
| --- | --- | --- | --- |
| add | `agent.sandbox.egress.rule_added` | `host`, `port` | Subsequent connections to that destination are allowed. |
| revoke | `agent.sandbox.egress.rule_revoked` | `host`, `port` | Subsequent connections are no longer allowed by the rule. |

Semantics:

- **Match is exact** on `host:port`, with the host compared case-insensitively.
  A pattern form (`*.example.com`, host-only any-port) is a possible extension,
  not part of this design.
- **Add is idempotent.** A rule already present is a no-op.
- **Revoke of an absent rule is a no-op**, not an error.
- **Revoke closes established tunnels** that the revoked rule granted. A
  permission change is a statement about the present, so leaving a tunnel open
  would contradict the recorded state. The alternative (drain in-flight bytes) is
  tracked in [Open Questions](#open-questions).
- A rule never grants more than its `host:port`. Adding one destination does not
  widen any other.

The allowlist is **not** durable state owned by the proxy. It is reconstructed
by replaying `agent.sandbox.egress.rule_*` events from the bus for the sandboxed
process, so the log remains the source of truth and a restarted proxy rebuilds
the same set. See [events.md](./events.md).

### Authority

Only a client holding the **authority** claim may publish `agent.sandbox.egress.*`
events. This is what prevents **self-approval**: an agent confined in the
sandbox cannot add a rule for itself, cannot approve its own request, and cannot
revoke a denial. The agent's only egress capability is to *ask* — by opening a
connection the proxy turns into a `requested` event.

The bus does **not** yet provide this claim as part of the wire protocol: its
access control is a UDS pathname permission plus a `SO_PEERCRED` UID/GID
allowlist, and a confined agent shares the daemon's UID, so peer identity cannot
distinguish an approver from an agent.

The bus now provides the claim **as a second listener** whose socket path the
sandbox withholds from the confined command. A connection on that listener holds
the authority capability; a connection on the ordinary listener does not. There
is no token to leak, sniff, or replay — the confined process never has the path,
and a process that never has it cannot obtain the claim. The bus gates the
decision and rule-change event types on it and refuses the rest with `forbidden`.
See [events.md](./events.md#authority) and
[README Open Questions](./README.md#open-questions) for the mechanism choice and
its residual assumptions.

## Per-Connection Approval

When a connection arrives for a destination not in the allowlist and an approver
is configured, the proxy asks instead of refusing.

```
confined command
  │  CONNECT host:port
  ▼
proxy ── published ──► agent.sandbox.egress.requested {request_id, host, port}
  │                                        │
  │  (proxy waits, bounded by a deadline)  ▼
  │                            approver (authority client)
  │  ◄── agent.sandbox.egress.granted ─────┤  same request_id
  │      agent.sandbox.egress.denied ──────┤
  │      agent.sandbox.egress.cancelled ───┘
  ▼
200 Connection Established   or   403 Forbidden
```

State machine for one connection:

| State | Trigger | Next |
| --- | --- | --- |
| `received` | request head read under the timeout | `authenticated` or `closed` |
| `authenticated` | `Proxy-Authorization` matches | `deciding` |
| `deciding` | destination is listed | `tunneling` |
| `deciding` | destination unlisted, no approver | `denied` |
| `deciding` | destination unlisted, approver configured | `awaiting` |
| `awaiting` | `granted` with the same `request_id` | `tunneling` |
| `awaiting` | `denied` / `cancelled` with the same `request_id` | `denied` |
| `awaiting` | deadline with no decision | `denied` (records `cancelled`) |
| `tunneling` | either direction ends, or revoke of its rule | `closed` |

Rules:

- **Correlation is by `request_id`** (a UUID). A decision that does not carry the
  request's id is ignored, so one request cannot release another, even with
  concurrent connections.
- **A deadline is a `cancelled`, not a `denied`.** The log must never claim an
  operator refused something they never saw. A cancellation published by an
  approver is also recorded as `cancelled`.
- **The approver's `granted` is the durable record.** The proxy does not
  republish it, so there is exactly one grant event per decision.
- **A listed destination never consults the approver.** The allowlist is the
  pre-approved set; approval is only for what is not on it.
- Approval is **opt-in**. Without a configured approver, an unlisted destination
  is denied, to avoid prompt fatigue.

## Interaction with the Event Bus

The event bus is the control and audit plane for both mechanisms. Nothing is
decided out of band:

- The proxy **subscribes** to the bus to receive `rule_added` / `rule_revoked`
  and the approval decisions.
- The proxy **publishes** `requested` (and its own `cancelled` on a deadline).
- An authority client **publishes** rules and decisions.
- Every event is durable and replayable in the WAL, so the audit trail is the
  log itself. See [events.md](./events.md) for the payloads and
  [event-bus delivery](../event-bus/delivery.md) for at-least-once semantics.

Because a confined command's *only* route out is the proxy, an unlisted
destination cannot be reached by bypassing the request. Detection is complete:
there is no path the proxy is not on.

## Testing Strategy

- **Allowlist**: the zero set denies; `rule_added` allows exactly its `host:port`
  and nothing else; `rule_revoked` denies subsequent connections; add and revoke
  are idempotent.
- **Revoke**: a tunnel open when its rule is revoked is closed.
- **Approval**: a listed destination never consults the approver; an unlisted
  destination with no approver is `403`; with an approver it is granted on
  `granted`, refused on `denied`, and refused on `cancelled`; a decision with a
  different `request_id` is ignored.
- **Deadline**: no decision within the timeout records a `cancelled` and answers
  `403`.
- **Authority**: a client without the authority claim cannot publish
  `agent.sandbox.egress.*`, so an agent cannot widen or approve its own reach.
- **Replay**: a restarted proxy rebuilds the same allowlist by replaying the
  rule events.
- **End to end**: an unlisted destination under an approver is granted and the
  tunnel carries bytes.

## Open Questions

The unresolved decisions for this topic are collected in
[README Open Questions](./README.md#open-questions): revoke semantics for
in-flight tunnels, the replay start point, pattern rules, approval grouping, the
approval latency default, and multiple approvers.
