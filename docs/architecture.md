---
type: Architecture
title: architecture
description: agentdのコンポーネント構成とイベントの流れ
tags:
  - architecture
  - agentd
  - CloudEvents
generated:
  by: human:temma.fukaya@mokmok.dev
  at: 2026-09-13T13:57:33Z
---

# agentd architecture

`agentd` is an always-on agent daemon. Components coordinate in a
choreography style: they publish events onto an in-process event bus and react
to the events published by others; there is no central orchestrator. Clients
connect over a WebSocket served on a Unix domain socket.

Every event is written to a durable append-only JSONL log **before** it is
fanned out, so the log is the source of truth and a live subscriber can never
see an event that is not already durable. Read models are rebuilt from the log
rather than from a second source of truth. Every event that leaves the log
carries its one-based position (`Seq`) alongside it, so a consumer can record
how far it has processed and resume from there.

## Components

```mermaid
flowchart LR
    P["WS client"] -- "WebSocket over UDS" --> S["axum server"]
    S -- "publish (await)" --> L["EventLog writer thread"]
    L -- "batch append + fsync" --> D[("events.jsonl<br/>append-only JSONL")]
    L -- "fan out in LSN order" --> B["EventBus<br/>(broadcast channel)"]
    B -- relay --> C["other WS clients"]
    D -. "replay (read_from)" .-> R["projections"]
    B -. subscribe .-> R

    style D fill:#f8f8f2,stroke:#888
```

| Component          | Crate           | Role                                                                                                              |
| ------------------ | --------------- | ----------------------------------------------------------------------------------------------------------------- |
| Unix domain socket | `agentd`        | Transport and single-instance boundary; a stale socket file is removed, a live one reports `AlreadyRunning`.       |
| WebSocket endpoint | `agentd`        | Ingress and egress protocol. Connections authenticate with a bearer token (see [Access control](#access-control)); inbound text frames must parse as CloudEvents and pass validation; outbound messages pair the event with its `seq` (see [Wire envelope](#wire-envelope)) and support `?from=` to resume. |
| `EventBus`         | `agentd-events` | Internal live fanout to subscribers via a `tokio::sync::broadcast` channel (capacity 1024 per subscriber); only `EventLog` publishes to it. |
| `EventLog`         | `agentd-events` | Durable write path: owns the writer thread, the log sequence number, the JSONL file, and the live fanout.           |
| JSONL log          | `agentd-events` | Append-only source of truth; one CloudEvents envelope per line, position = one-based line number (`Seq`).          |
| Projections        | `agentd-events` | Read models derived from the log, resuming from an `applied_seq` checkpoint (`agentd_events::projection`).          |
| Node               | `agentd-node`   | Long-lived client that consumes `/events` and keeps a SQLite projection current (see [node](node.md)).               |

## Event model

Every event on the bus and in the log is a CloudEvents 1.0 envelope:

| CloudEvents attribute | Rust field    | Value / format                                              |
| --------------------- | ------------- | ----------------------------------------------------------- |
| `id`                  | `id`          | UUID version 7, so IDs order by creation time                |
| `source`              | `source`      | `urn:mokmokd` for daemon-produced events                     |
| `specversion`         | `specversion` | `1.0`, validated on ingress                                  |
| `type`                | `r#type`      | dotted type, e.g. `error.lagged`, `error.publish_failed`    |
| `time`                | `time`        | RFC 3339 UTC; optional per the specification                 |
| `data`                | `data`        | Arbitrary JSON payload                                       |

```json
{
  "id": "0199b7ea-8f4a-7d12-9c3a-2f8b1e4d6a90",
  "source": "urn:mokmokd",
  "specversion": "1.0",
  "type": "test.event",
  "time": "2026-09-13T12:00:00.123456Z",
  "data": { "value": 1 }
}
```

### Wire envelope

Outbound WebSocket messages pair the event with its position:

```json
{
  "seq": 42,
  "event": {
    "id": "0199b7ea-8f4a-7d12-9c3a-2f8b1e4d6a90",
    "source": "urn:mokmokd",
    "specversion": "1.0",
    "type": "test.event",
    "time": "2026-09-13T12:00:00.123456Z",
    "data": { "value": 1 }
  }
}
```

`seq` is the one-based log position, or `null` for a transient daemon notice
that is not part of the log (for example `error.lagged`). The position is
transport metadata and is never written into the JSONL log, whose lines remain
plain CloudEvents. Inbound frames are a bare CloudEvent; the daemon assigns the
position on append.

A consumer's saved position (its cursor) must only advance on a message whose
`seq` is non-null; notices carry `null` and must not overwrite it.

## Access control

The daemon is not an open bus. Every connection presents an
`Authorization: Bearer <token>` header whose token maps to a set of claims:

| Claim       | Grants                                                                              |
| ----------- | ----------------------------------------------------------------------------------- |
| `read`      | Subscribe to the stream (replay and live).                                            |
| `publish`   | Append non-reserved events.                                                           |
| `authority` | Publish the reserved daemon-authority types `error.*`, `sandbox.*`, and `session.*`.  |

A read-only connection cannot append; a publish attempt is answered with an
`error.unauthorized` notice. A token without `authority` cannot publish a
reserved type, so a client cannot forge `sandbox.permission.granted`/`denied`
and hijack the approval flow. The daemon overwrites the client-supplied
`source` (with the authenticated principal's) and `time` (with its own), and
validates `specversion`, `type`, and `id`, so an event's provenance in the log
is daemon-owned.

Tokens live in a JSON file (`--token-file`, default `~/.agentd/tokens.json`)
that must not be readable or writable by group or other users; the daemon
refuses to start otherwise. The socket directory and protocol files default to
`~/.agentd` and are created mode `0700`/`0600`. The token is a **capability**:
strong isolation from a compromised same-uid agent depends on the agent running
inside the sandbox with the token file in `deny_read` (see
[sandbox](sandbox.md)) — wiring the sandbox is the remaining step.

## Sequences

### Publishing and relaying an event

```mermaid
sequenceDiagram
    participant P as Publisher (WS client)
    participant S as Server
    participant L as EventLog
    participant C as Subscriber (WS client)

    P->>S: text frame (CloudEvents JSON)
    S->>S: parse and validate specversion
    alt invalid message
        S-->>P: error.invalid_event event
    else valid event
        S->>L: publish(event)
        L->>L: append + fsync
        alt append failed
            L-->>S: Err
            S-->>P: error.publish_failed event
        else append committed
            L-->>S: Ok(seq)
            L-->>C: fan out { seq, event }
        end
    end
```

### Durable append (group commit)

```mermaid
sequenceDiagram
    participant S as Server
    participant Q as writer queue (bounded)
    participant W as Writer thread
    participant D as events.jsonl

    S->>Q: publish(event) (awaits an ack)
    W->>Q: drain the queue into one batch
    W->>D: write one JSON line per event
    W->>D: fsync once for the whole batch
    W->>W: fan out in LSN order, then ack each publish
    Note over W,D: one write and one fsync per batch; concurrent publishers share the fsync
```

The writer thread owns the file and the sequence number, so the log order is
the delivery order. `publish` returns only after the append has been synced;
events that cannot be synced are reported as errors instead of being silently
dropped.

### Backpressure and lag

The writer queue is bounded, so a slow disk backpressures publishers rather
than growing memory or losing events. Live subscribers are ordinary broadcast
subscribers: a subscriber that cannot keep up loses its live stream and
observes an `error.lagged` event, but the log is unaffected because it was
written before the fanout.

### Resuming from a position

A consumer persists the last `seq` it applied and reconnects with
`GET /events?from=<seq>` (inclusive). The server subscribes to the live bus
first, replays history from `from` to the end of the log, then continues live,
skipping any event it already sent:

```mermaid
sequenceDiagram
    participant C as Consumer (reconnect)
    participant S as Server
    participant L as EventLog
    participant B as EventBus

    C->>S: GET /events?from=42
    S->>B: subscribe (buffer live)
    S->>L: read_from(42)
    L-->>S: entries 42..N
    S-->>C: wire { seq, event } for 42..N
    B-->>S: live events (some already replayed)
    S-->>C: wire { seq, event } for events after N (duplicates skipped)
```

If the live buffer overflows during a long replay, the server re-reads the
durable tail from the last sent position instead of dropping events, so a
resumed consumer never silently skips a position. A position outside
`1..=tail+1` (with `tail` the last committed position) is answered with an
`error.resume_out_of_range` notice and the connection is closed; a replay
failure likewise closes after an `error.replay_failed` notice, because
continuing would leave a permanent gap. Without `from`, a consumer is live-only
and a lag is reported as `error.lagged`, as before.

### Startup and recovery

```mermaid
sequenceDiagram
    participant M as main
    participant E as EventLog
    participant D as events.jsonl
    participant S as Server

    M->>E: open(log_path)
    E->>D: create parent dir, open (create if missing)
    E->>D: stream complete lines, truncate a partial trailing line
    E->>E: resume the sequence after the last complete line, spawn writer thread
    M->>S: run(socket, log)
```

## Log format and recovery strategy

The log is newline-delimited JSON. Each line is the verbatim CloudEvents
envelope, so the schema is stable when extension attributes appear and the log
stays consumable by ordinary tooling (`jq`, `rg`, DuckDB). There is no
migration list: attributes are read on demand.

- The position of a line is its one-based line number, exposed as `Seq`.
- A crash mid-append can leave a partial trailing line; recovery truncates it
  and keeps every complete line, so the log always ends on a line boundary.
- The log is at-least-once across a writer failure: an event that was written
  but not acknowledged may survive, so a retrying publisher can append it
  twice. Consumers deduplicate by `Event::id`.
- Positions are not stored in the file; they are derived from line numbers and
  attached as `agentd_events::LogEntry` on the live bus and the WebSocket API,
  so the file stays a plain CloudEvents stream. A consumer resumes by passing
  the last position it applied to `GET /events?from=`.
- History queries read the log directly (DuckDB over the JSONL, or `rg`);
  stateful read models replay it through `projection::catch_up`, which resumes
  from the projection's `applied_seq`.

## Nodes

A node is a long-lived client that consumes the log and keeps a SQLite
projection current (see [node](node.md)). It connects to `/events` with
`?from=<checkpoint + 1>`, selects the events it cares about, and writes the
reducer's state change and the checkpoint in **one SQLite transaction**, so a
restart resumes without gaps or duplicates. An event the node is not interested
in still advances the checkpoint, so it is not re-read.

The JSONL log is the permanent event store; the SQLite file is a projection that
can always be rebuilt from it. The log is not a write-ahead log and is never
truncated to a checkpoint, and the projection is not a second source of truth.

## Extension model

The daemon is extended out of process. An extension consumes and produces
CloudEvents 1.0 JSON over the WebSocket event API (a Unix socket), authenticating
with a bearer token that carries the claims it needs; any external program in any
language qualifies and no crate is required. There is no
in-process extension mechanism: components that need in-process access to the
event contract are first-class crates, not plug-ins.

The current structure follows these rules:

- `agentd-events` holds the event contract (`Event`, `SPEC_VERSION`,
  `DAEMON_SOURCE`), the live fanout (`EventBus`), and the durable log
  (`EventLog`) with its projection helpers. Every other crate depends on it
  and must never depend on the `agentd` binary crate.
- The durable JSONL event log is core: it lives in `agentd-events`, is the
  source of truth, and is never feature-gated.
- `agentd-node` depends on `agentd-events` only and is neither the daemon binary
  nor an extension: it is the client-side node library and binary, run out of
  process (and eventually inside the sandbox; see [node](node.md)).
- `agentd-sandbox` is a first-class crate like `agentd-node`: it depends on
  `agentd-events` only, and `agentd` exposes it behind the optional
  `sandbox = ["dep:agentd-sandbox"]` feature.
- First-class crates document their deviations from their design docs next to
  the code that embodies them, so a reader never has to reconcile two sources of
  truth from memory.

`agentd-sandbox` (see `docs/sandbox.md`) is the confinement layer through which
an agent drives shell commands. It holds an `EventLog`, durably appending
`sandbox.permission.*`, `sandbox.violation.*`, and `sandbox.exec.completed` for
every decision and OS denial, so approval flows are ordinary subscribers and no
decision is lost. `Sandbox::exec` returns a `Result` and surfaces
`SandboxError::Publish` when an append fails; the decision is appended before a
command runs, so a command never starts without its decision recorded. The
boundary is the platform's native isolation (Seatbelt on macOS;
bubblewrap-preferred with a Landlock fallback on Linux), and the sandbox has no
network egress: inference is a daemon capability, reached over the daemon's Unix
socket, which is the only endpoint a confined command may connect to. Its
layer-1 executor is macOS-only so far (the Linux backend is the follow-up), the
macOS profile renders the policy's path entries with protected metadata and
opens no network, and the `sandbox` feature also carries a session manager
(`agentd::session`) that launches a configured sandboxed node on a
`session.requested` event, reports `session.*` lifecycle, restarts a crashed
node within a budget, enforces a session lifetime, and answers a status request.
The binary starts the manager when `--session-command` (with `--sandbox-policy`)
is given; without it the feature only validates the dependency graph under CI's
`--all-features`.

Further structural steps keep explicit triggers and are not taken early:

1. An external Rust consumer of the contract appears: start versioning and
   publishing `agentd-events`.
2. Out-of-process consumers want convenience: add a thin client crate; the
   wire format itself is already stable.

Adding workspace members requires no build-infrastructure changes: crane
discovers members from the workspace manifest, and CI runs clippy with
`--all-features` and tests over the whole workspace.