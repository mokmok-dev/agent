# Architecture

## Boundaries

Four boundaries carry untrusted data. Each one parses to a typed value before
the rest of the code sees it.

| boundary                           | who parses                 | on bad input                                 |
| ---------------------------------- | -------------------------- | -------------------------------------------- |
| provider webhook                   | `packages/adapters`        | 401 for a bad signature, 400 for a bad body  |
| ACP frame from the agent           | `packages/protocol`        | the frame is dropped; the turn keeps running |
| Durable Object storage             | `packages/store`           | a missing session raises `MissingSession`    |
| control-plane frame at the gateway | `apps/gateway/src/edge.ts` | the frame is ignored                         |

Inside those boundaries the code trusts its types. There is no second check of
a value that was already decoded.

## One owner per thing

- A conversation is one `SessionDurableObject`. Cloudflare serializes calls to
  one object, so a delivery never interleaves with another.
- A thread maps to one session id through `ThreadIndexDurableObject`. Two
  webhooks racing for one thread cannot both create a session.
- A turn is one record with one status. The legal moves live in
  `TURN_TRANSITIONS`, and `transitionTurn` is the only writer.
- The gateway owns the ACP client for its VM. The control plane never speaks
  ACP.

## Why the VM dials out

The gateway opens one outbound WebSocket. The VM needs no inbound port, so a
compromised control plane cannot reach other services on the VM, and the VM
needs no public address.

Events are appended to the local log before they are sent. On reconnect the
gateway sends everything unacknowledged, so a dropped connection loses
nothing.

## Idempotency

Every state change converges when it runs twice.

- A delivery carries the provider's event id. The idempotency key stores the
  turn it created, so a replay is one storage read.
- A turn transition derives the next turn from the stored turn. Repeating a
  rejected move changes nothing.
- The gateway replays its log after a reconnect. A duplicate event is harmless
  because the control plane keys turns by the provider's event id.

## Failure behaviour

| failure                      | result                                    |
| ---------------------------- | ----------------------------------------- |
| provider retries a webhook   | same turn, no duplicate                   |
| the agent process crashes    | supervisor restarts it, up to three times |
| the agent keeps crashing     | the turn fails, the session stays open    |
| the WebSocket drops          | events replay on reconnect                |
| a permission request arrives | the turn waits for the person             |
| a malformed agent frame      | the frame is dropped, the turn continues  |

## Testing

Every package holds a mutation gate at 100% killed. The tests are property
tests where a rule has invariants (transition legality, replay, thread keys,
signature round trips) and example tests where a rule is one branch
(diagnostics, adapter fields).

`apps/gateway/src/e2e.test.ts` runs the real scripted agent over real stdio, so
the codec, the supervisor, and the bridge are exercised together.
