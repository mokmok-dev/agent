---
type: Design
title: events
description: CloudEvents written to the event bus for sandbox permission decisions, egress changes, and violations
tags:
  - sandbox
  - eventbus
  - CloudEvents
  - permission
  - violation
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Sandbox Events

Every permission decision, every egress change, and every policy violation is a
durable CloudEvent on the [event bus](../event-bus/README.md). The audit trail is
the log itself; there is no separate side channel.

The envelope follows [event-bus CloudEvents](../event-bus/cloudevents.md): the
bus assigns `source`, `id`, `time`, and `sequence` at commit. Producers set
`type`, `subject`, and `data`.

## Attribute Conventions

| Attribute | Use |
| --- | --- |
| `type` | The dotted type below, e.g. `agent.sandbox.permission.requested`. |
| `subject` | The `sandbox_id`, so a consumer can filter one sandbox's stream. |
| `data.sandbox_id` | Repeated in the payload for correlation with `request_id`. |
| `data.request_id` | A UUID correlating one request with exactly one decision. |
| `traceparent` | W3C Trace Context, propagated so a tunnel and its approval share one trace. |

The payload names the command under `data.command` when a command is involved.
`subject` is reserved for the `sandbox_id`, so one word does not name both the
command and the sandbox.

## Permission Events

A permission check produces a `requested` and exactly one terminal outcome.

| Type | When |
| --- | --- |
| `agent.sandbox.permission.requested` | A command or resource access needs a decision. |
| `agent.sandbox.permission.granted` | A static rule or an approver allowed it. |
| `agent.sandbox.permission.denied` | A static rule or an approver refused it (deny wins). |
| `agent.sandbox.permission.cancelled` | Nobody decided before the deadline, so it was withdrawn. |

- `requested` carries `decision`, either `auto` (a static rule granted
  immediately) or `pending` (human approval is configured).
- An approver reacts to `requested` by publishing exactly one of `granted`,
  `denied`, or `cancelled` with the **same `request_id`**. A decision with a
  different id is ignored.
- With `auto`, the sandbox publishes `requested(auto)` and then `granted`
  itself. With an approver, the approver's `granted` is the durable record and
  is not republished.
- A timeout records a `cancelled` and returns a denied result, so the log never
  claims an operator refused something they never saw.

## Egress Events

The egress proxy and its authority clients use these. See
[permissions.md](./permissions.md) for the state machine.

| Type | When |
| --- | --- |
| `agent.sandbox.egress.requested` | A connection arrived for an unlisted destination and an approver is configured. |
| `agent.sandbox.egress.granted` | An approver allowed the destination; the proxy opens the tunnel. |
| `agent.sandbox.egress.denied` | An approver refused the destination; the proxy answers `403`. |
| `agent.sandbox.egress.cancelled` | The deadline passed with no decision, or an approver withdrew it; the proxy answers `403`. |
| `agent.sandbox.egress.rule_added` | An authority added a destination to the mutable allowlist. |
| `agent.sandbox.egress.rule_revoked` | An authority removed a destination; matching tunnels are closed. |

Payload for a destination decision: `request_id`, `host`, `port`, and the
optional `traceparent`. Payload for a rule change: `host`, `port`, and the
`sandbox_id` whose proxy owns the set.

The four decision types (`granted`, `denied`, `cancelled`, and both rule-change
types) require the [authority](#authority) claim, so an agent cannot widen or
approve its own reach. The proxy is itself an authority principal: it publishes
`requested`, and its own deadline `cancelled`, as the trusted daemon.

The mutable allowlist is reconstructed by replaying `rule_added` /
`rule_revoked` events, so the log — not the proxy — is the source of truth for
what a sandbox is permitted to reach.

## Violation Events

An OS-enforced denial becomes a structured, durable event instead of a log line.

| Type | When |
| --- | --- |
| `agent.sandbox.violation.filesystem` | The kernel refused a filesystem operation. |
| `agent.sandbox.violation.network` | The kernel refused a network operation. |

Payload:

| Field | Meaning |
| --- | --- |
| `reason` | The classification, e.g. `OperationNotPermitted`, `ReadOnlyFileSystem`, `PolicyDenied`, `SignalSyscall`. |
| `denied_path` | The path involved, when known. |
| `output` | A bounded snippet of the command's stderr, so the event is self-contained. |

A violation is emitted only for a recognised OS denial; an unrelated non-zero
exit is not a violation. The classifier maps the exit code and output to the
`reason`.

## Execution and Process Events

| Type | When |
| --- | --- |
| `agent.sandbox.exec.completed` | Terminal state of a one-shot execution: exit code, duration, output sizes. |
| `agent.sandbox.process.started` | A long-lived confined process was spawned. |
| `agent.sandbox.process.exited` | Terminal state of that process: exit code and duration. |

A long-lived process is confined by one policy and its children inherit it, so
per-command `permission` events are not emitted for it; its lifecycle events are
the audit trail. Approval for a spawned process happens once, at spawn.

## Authority

Approval and allowlist events require a client token carrying the **authority**
claim. A client without it cannot publish:

- any `agent.sandbox.egress.*` event, so an agent cannot add a rule for itself or
  approve its own request;
- any `agent.sandbox.permission.*` decision event, so an agent cannot approve its
  own command.

This is the guarantee that keeps a human meaningfully in the loop: the confined
agent's only capability is to *ask*.

Peer credentials alone are not enough. The bus's only access control today is a
UDS pathname permission plus a `SO_PEERCRED` UID/GID allowlist
([event-bus security](../event-bus/security.md)), and a confined agent runs as
the daemon's user, so it is the *same* peer UID as its approver. The bus must
therefore gate these event types on a publisher-supplied claim, not on the peer
identity. If it does not, an agent on the same UID could publish its own
`granted` and the boundary would fail silently. This is a dependency on the bus,
not a property the sandbox can enforce by itself; it is recorded in
[README Open Questions](./README.md#open-questions).

## Example Exchange

A confined agent reaches an unlisted host; an approver grants it.

```json
{"specversion":"1.0","type":"agent.sandbox.egress.requested",
 "source":"agent://eventbus","id":"01J8Z...","subject":"sbx-7",
 "time":"2026-09-27T00:00:00Z",
 "traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
 "data":{"request_id":"req-9","host":"api.example.com","port":443}}
```

```json
{"specversion":"1.0","type":"agent.sandbox.egress.granted",
 "source":"agent://eventbus","id":"01J8Z...","subject":"sbx-7",
 "time":"2026-09-27T00:00:01Z",
 "data":{"request_id":"req-9","host":"api.example.com","port":443}}
```

The proxy opens the tunnel and the agent completes its request; the two events
above are the durable record of who allowed what, and when.

## Testing Strategy

- **Correlation**: a decision with a mismatched `request_id` does not release a
  request.
- **Outcomes**: `requested` is followed by exactly one of `granted`, `denied`,
  or `cancelled`; a deadline records `cancelled`, not `denied`.
- **Authority**: a client without the authority claim cannot publish an
  `agent.sandbox.egress.*` or `agent.sandbox.permission.*` event.
- **Violation classification**: a recognised OS denial produces a structured
  `violation.*` with a `reason`; an unrelated failure produces none.
- **Replay**: replaying `rule_added` / `rule_revoked` reconstructs the same
  allowlist.
- **Trace**: the confined request's `traceparent` reaches its `requested` and
  terminal decision events.
