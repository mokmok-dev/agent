---
type: Design
title: session
description: セッションマネージャを、サードパーティツールをサンドボックス実行し IPC と死活監視を行い、子プロセスと CloudEvents の変換レイヤを Bridge として外に分離する設計。role 分解、命名、イベントの帰属、ルーティング、バックプレッシャを定める。
tags:
  - session
  - bridge
  - sandbox
  - IPC
  - cloudevents
  - eventlog
generated:
  by: human
  at: 2026-09-19T00:00:00Z
---

# agentd session design

The daemon supervises third-party tools: it runs them confined by an OS sandbox,
keeps them alive under a restart/lifetime policy, and gives them a face on the
event bus so they participate in the choreography even though they do not speak
CloudEvents themselves. This document fixes the roles that make that up and the
names for them, so the conversion layer does not become a property of the
supervisor.

## Goals and non-goals

Goals:

- A third-party tool (an MCP server, a language server, a CLI) can be run as a
  confined, supervised child process and take part in the event choreography.
- The supervisor stays protocol-agnostic: adding a protocol does not touch it.
- Process ownership, OS confinement, and protocol conversion are three
  orthogonal roles with separate contracts.
- The daemon's tokens and the `authority` claim stay on the daemon side; a child
  never holds network credentials and never approves its own command.

Non-goals:

- Turning children into event-bus peers. A child that speaks CloudEvents over the
  daemon's WebSocket (`agentd-node`) is already a first-class participant and
  needs no Bridge; that path is unchanged.
- A per-session mailbox with its own routing table. The durable log is the
  mailbox; the bus is the delivery surface. A second routing plane is not built.
- A worker pool. Third-party tools are not interchangeable — each has its own
  protocol — so they are supervised individually, not pooled.

## Roles

Three roles, orthogonal, one contract each. The point of the split is that the
conversion layer can be swapped without touching supervision, and supervision
can be swapped without touching conversion.

| Role              | Contract                          | Crate            |
| ----------------- | --------------------------------- | ---------------- |
| Confinement       | "given a `Policy`, run something isolated" | `agentd-sandbox` |
| Supervision       | "own one child: spawn, liveness, restart, lifetime" | `agentd` (`session`) |
| Conversion        | "translate between a byte stream and CloudEvents" | `agentd` (`session`) |

The bus is the actor system. The log is the mailbox; `EventBus` is the fanout.
A node that speaks CloudEvents is a first-class actor on it. A third-party tool
is a peripheral device with a fixed protocol; a **`Bridge`** is the facade that
gives it an actor-shaped face. So the actor is the Bridge (and the bus), never
the child.

## Names

"Session" currently names two different things, which is the root of the naming
difficulty:

- `agentd_sandbox::Session` is a long-lived confined process with piped stdio.
  It has no type name at the manager level (`agentd::session` names the manager,
  not the unit).
- `agentd::session::SessionManager` supervises those units.

The fix is to make the unit structural:

```rust
/// The unit the manager owns. It knows nothing about protocols.
pub struct Session {
    process: SandboxedProcess, // renamed from agentd_sandbox::Session
    bridge: Box<dyn Bridge>,
}
```

- `agentd_sandbox::Session` → **`SandboxedProcess`**: the child process, its
  pipes, and the profile/scratch it was spawned under. This is the mechanism.
- **`Session`** = `SandboxedProcess` + `Bridge`. The manager's unit of
  supervision. Its `Bridge` field is `dyn`, so the manager never names a
  protocol.
- **`Bridge`** = the conversion. Bidirectional and symmetric, so one word
  covers both directions.

`Bridge` is chosen over the alternatives for three reasons: the direction is
symmetric (a child-to-bus *uplink* and a bus-to-child *downlink* are both
"bridging"), the concrete type can be named after the protocol
(`StdioBridge`, `McpBridge`, `LspBridge`), and the name does not claim the
manager knows the protocol. `Driver` is avoided because it collides with the
tokio I/O driver; `Adapter` and `Codec` were considered and lose the symmetry or
sound byte-level.

Vocabulary that is reserved, not used here:

- **actor** — the bus, the log, and CloudEvents-speaking nodes. Not children.
- **worker** — a later, interchangeable pool over one protocol. Nothing today
  is interchangeable.

Child-side nouns, to be chosen deliberately when a consumer appears: `Tool`
if "third-party tool" is the protagonist, `Workload` if the generality of
confinement is. The name should not imply more capability than the child has.

## Bridge contract

```rust
pub trait Bridge: Send + Sync {
    /// Child bytes -> events. Called as the child produces output.
    async fn uplink(&self, ...) -> Option<Event>;
    /// Events -> child bytes. Called for each event routed to the session.
    async fn downlink(&self, event: Event);
}
```

`uplink` is driven by the child's output; `downlink` by the bus. The manager
gives a `Bridge` the child's pipes (`SandboxedProcess::take_stdin` /
`take_stdout` / `take_stderr`, see [sandbox](sandbox.md)) and the publish
handle. It never inspects the bytes.

A `Bridge` is per-protocol. A tool with no protocol at all gets a minimal
`StdioBridge` that turns stdout into framed events; the richer bridges
(`McpBridge`, `LspBridge`) implement real framing and correlation.

## Event provenance

The daemon appends events on behalf of a child, so `source` alone cannot say who
produced them. The bus stays single-sourced at `urn:mokmokd`
(`DAEMON_SOURCE`), and identity moves to `subject`:

| Attribute | Value                                                            |
| --------- | --------------------------------------------------------------- |
| `source`  | `urn:mokmokd` — everything on this log is daemon-mediated        |
| `subject` | the session id the child runs under (`session:<id>`)             |
| `type`    | `session.*` lifecycle, or a protocol-family prefix for payloads  |

A `Bridge` stamps `subject` from its session on every event it emits, so a
consumer can route or filter by session without a second routing table. Giving
each child its own `source` was rejected: provenance would then live in two
places (source for children, subject for everything else), and the daemon would
lose the ability to say "the daemon mediated this".

`subject` is an optional CloudEvents attribute and `Event` does not carry it
today (`agentd-events/src/lib.rs:86`), so this is an additive field with a
serde default and a `skip_serializing_if`, the same shape as `time`. It is not
part of `set_provenance`: the daemon owns `source` and `time` on ingress, but
`subject` belongs to the producer (the Bridge) and must survive a relay.

## Routing and backpressure

Downlink reuses the existing client-side selection: a session declares the
event types it wants — the `Interest` / `TypePrefixes` model in
`agentd-node/src/filter.rs` — and the manager, or the `Bridge`, drops anything
else before writing a byte to the child. The child does not get a mailbox of its
own; the log already holds every candidate, and downlink is just a filtered
projection of it.

Uplink must not flood the log. A `Bridge` frames the child's stream and applies
an output budget equivalent to the sandbox's one-shot `max_output_bytes`; a
raw stdout that never yields a frame is capped, not appended forever. Framing
and capping are the Bridge's job precisely because only the Bridge knows what a
"complete message" is.

## Relationship to the existing session manager

`agentd::session::SessionManager` already owns process liveness, restart, and
lifetime against the durable log, and reconciles on startup by failing any
session the previous daemon left open (see [sandbox](sandbox.md#sessions)). It
stays the supervisor. What changes is only that its unit becomes
`Session { SandboxedProcess, Bridge }` instead of a raw process, so supervision
and conversion stop being entangled.

`agentd-node` is deliberately *not* downgraded to a stdio child. It already
speaks CloudEvents and authenticates as a peer, so it needs no Bridge and the
two models would otherwise duplicate. A Bridge is for tools that cannot speak
the protocol, and only those.

## Testing strategy

- **Bridge unit tests**: framing per protocol, a partial frame buffered across
  reads, an oversized frame capped rather than accumulated, and `subject`
  stamped on every emitted event.
- **Routing tests**: a downlink event with an uninteresting type reaches no
  byte; an interesting one reaches the child; `Interest` and the Bridge agree.
- **Separation test**: adding a stub protocol bridge requires no change to
  `SessionManager` — the manager stays `dyn Bridge`.
- **Supervision regression**: existing `agentd::session` tests (lifecycle,
  restart budget, lifetime kill, status, startup reconciliation) still pass with
  the unit renamed to `Session`.

## Implementation status

Design only. No code in this document is implemented yet. The refactors it
implies are: rename `agentd_sandbox::Session` to `SandboxedProcess`, introduce
the `Bridge` trait and the `Session { SandboxedProcess, Bridge }` unit, and
define `subject` stamping for bridged events. The existing supervisor, its
`session.*` lifecycle events, and the `agentd-node` CloudEvents peer path are
unchanged by this design.
