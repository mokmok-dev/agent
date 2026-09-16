---
type: Design
title: node
description: agentd のイベントログを購読し SQLite に射影する長命ノード(セッション)の設計。チェックポイント、冪等な再適用、sandbox 上での実行方針を定める。
tags:
  - node
  - session
  - projection
  - sqlite
  - eventlog
generated:
  by: human
  at: 2026-09-16T00:00:00Z
---

# agentd node design

A node is the client side of the choreography. It is a long-lived process that
subscribes to the daemon's event log over a WebSocket on a Unix domain socket,
decides which events it cares about, and keeps a SQLite projection in sync with
them. The eventual target is a **session** running inside a sandbox; the first
version is sandbox-agnostic so that constraint does not force a rewrite later.

## The store/projection split

Two layers, with a strict direction of truth:

| Layer                | Lives in       | Role                                                               | Disposable? |
| -------------------- | -------------- | ------------------------------------------------------------------ | ----------- |
| JSONL event log      | `agentd-events` | Append-only CloudEvents; the source of truth; never rewritten       | No          |
| SQLite projection    | `agentd-node`  | State derived by replaying the log; a read model with a checkpoint  | Yes         |

The SQLite file is **not** a write-ahead log and the JSONL log is **not** a
transient journal. A WAL is a database-internal, truncatable structure; the
JSONL log is the permanent event store. If the projection's schema or reducer
changes, delete the SQLite file and rebuild it from the log — the log is never
truncated or compacted in this design.

## Components

| Component          | Type                 | Role                                                                                   |
| ------------------ | -------------------- | -------------------------------------------------------------------------------------- |
| `WsClient`         | `agentd-node`        | Bidirectional WebSocket over a Unix domain socket; `?from=` resumes from a position.    |
| `Interest`         | `agentd-node`        | Client-side selection: whether an event should be applied.                              |
| `SqliteReducer`    | `agentd-node`        | The domain: the schema and how one event changes state.                                 |
| `SqliteProjection` | `agentd-node`        | Owns the connection, the checkpoint, and the transaction that keeps the two consistent. |
| `Node`             | `agentd-node`        | Connect, resume, select, apply, reconnect with backoff.                                 |

```mermaid
flowchart LR
    D[("events.jsonl<br/>event store (truth)")] -- "replay + live" --> S["agentd<br/>WebSocket /events"]
    S -- "WireMessage { seq, event }" --> C["WsClient"]
    C --> N["Node"]
    N -- "interested?" --> I["Interest"]
    N -- "reduce + checkpoint (one tx)" --> P[("SQLite projection<br/>rebuildable read model")]
    N -. "publish Event" .-> C -.-> S
```

## Checkpoint and idempotency

The checkpoint is the position of the last event the node applied **or
skipped**. It is stored in the same SQLite database as the projection state and
is written in the **same transaction** as the reducer's change. A crash can
therefore never leave the checkpoint ahead of the state it describes, so a
restart never silently skips an event.

Delivery is at-least-once: an event written but not acknowledged may be appended
twice after a crash. The node guards against duplicates by position — an event
at or before the checkpoint is ignored — so replaying an overlapping range is a
no-op. Reducers should still be written to be idempotent per event where the
domain allows it.

```mermaid
sequenceDiagram
    participant N as Node
    participant S as agentd
    participant P as SQLite

    N->>P: read checkpoint (applied_seq)
    N->>S: GET /events?from=applied_seq+1
    S-->>N: replay applied_seq+1 .. tail
    S-->>N: live events
    loop each event
        alt interested
            N->>P: BEGIN; reduce; checkpoint; COMMIT
        else skipped
            N->>P: BEGIN; checkpoint; COMMIT
        end
    end
```

## Filtering

The daemon broadcasts every event to every subscriber. The node decides with an
`Interest`:

- `TypePrefixes::new(["sandbox."])` matches by `CloudEvents` `type` prefix; an
  empty set matches everything.
- Any `Fn(&Event) -> bool` is an `Interest`.

A filtered-out event still advances the checkpoint, so a restart does not
re-read it. The consequence is that **changing the filter does not retroactively
apply events that were skipped**; to reproject under a new filter, rebuild the
projection with `--rebuild`.

## Running a node

```sh
agentd-node \
  --socket ~/.agentd/agentd.sock \
  --db /path/to/session/node.db \
  --token-file /path/to/node.token \
  --source urn:mokmokd:session:1 \
  --type-prefix sandbox.
```

`--token-file` is a file whose contents are the bearer secret the daemon
expects; the node needs only the `read` claim unless it publishes. Keep the file
private to the node's user.

The bundled `agentd-node` binary projects a count of events per `type` and exists
to validate the transport, checkpoint, and resume behaviour.

## The agent node

`agentd-agent` is the node a real session runs. It keeps the same connection,
checkpoint, and resume behaviour, but its reducer is the conversation read model
and it *reacts*: when an `agent.inbox` event for its conversation is applied, it
asks the daemon for a completion on `/inference`, runs the `shell` tool for any
tool call, and publishes the finalized messages as `agent.*` events (see
[inference](inference.md)).

```sh
agentd-agent \
  --socket /abs/path/agentd.sock \
  --db /path/to/session/agent.db \
  --token-file /path/to/agent.token \
  --conversation <id> \
  --workdir /path/to/workspace \
  --source urn:mokmokd:agent
```

The agent's token carries `read`, `publish`, and `infer` — never `authority`,
so it cannot forge daemon-authority events. It holds no provider credentials:
the daemon performs inference. Its `agent.*` types are non-reserved, so it can
publish them with the `publish` claim alone.

## Sandbox deployment

Nodes run inside the sandbox. Everything external is a CLI argument
(`--socket`, `--db`, `--token-file`, `--workdir`); the node assumes no host paths
and does not read the environment for config. Its stdout and stderr carry
diagnostics, not state.

The daemon's session manager (`agentd::session`) launches a configured node on a
`session.requested` event, reports `session.*` lifecycle, restarts within a
budget, enforces a lifetime, and reconciles the durable log on startup. The
sandbox's long-lived `Sandbox::spawn` runs the whole node under one profile;
because child processes inherit it, the agent's `bash` tool runs confined and no
per-command `sandbox.permission.*` events are emitted — the session's lifecycle
events are the audit trail.

The node's connection to the daemon is the one allowed network path: egress is
denied outright, and the daemon grants its own Unix socket to the session policy
(`network.unix_sockets`), rendered as a path-scoped Seatbelt grant. Because the
sandbox overrides `HOME` to its scratch directory, the node's default
`~/.agentd/agentd.sock` resolves to the scratch path, so `--session-command`
must pass the daemon's absolute `--socket`, `--token-file`, and `--db`. SQLite
writes its journal sidecar next to the database, so the whole **directory** must
be writable: place the database in one session `write` entry.

## Stated gaps

- **Synchronous SQLite.** A reducer's transaction runs on the async task that
  receives the event, so a slow disk blocks that worker. There is one commit
  (and its `fsync`) per event. Batching commits and offloading them to a
  blocking worker are deliberate follow-ups, not done yet because the node's
  only work is this stream and correctness is easier to see with one event per
  transaction.
- **A projection ahead of the log is fatal.** If the daemon starts with a fresh
  log, the node's checkpoint can be beyond the tail. The daemon answers
  `error.resume_out_of_range` and the node stops with
  `NodeError::ResumeOutOfRange`; rebuild with `--rebuild` rather than silently
  resetting state.
- **Changing the filter does not reproject.** Events skipped under an earlier
  filter are not revisited; rebuild the projection to apply a new filter.

## Testing

- Unit: checkpoint persistence, idempotent re-apply, skip semantics, and
  `Projection::catch_up` over an `EventLog`.
- End-to-end (`agentd/tests/node_session.rs`): a real daemon, a node projecting
  into SQLite, a filtered-out event advancing the checkpoint, and a restart
  resuming from the checkpoint without replaying from zero or double-counting.
- End-to-end (`agentd/tests/agent_loop.rs`): a real daemon with a scripted
  provider, a user prompt applied as `agent.inbox`, the agent running the shell
  tool, and the conversation and `agent.*` events appearing in the projection
  and the log.