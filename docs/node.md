---
type: Design
title: node
description: agentd のイベントログを購読し SQLite に射影する長命ノードの設計。チェックポイント、冪等な再適用、sandbox 上での実行方針を定める。
tags:
  - node
  - conversation
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
them. The eventual target is a node running as a supervised session inside a
sandbox; the first version is sandbox-agnostic so that constraint does not force
a rewrite later.

The nouns are fixed in [vocabulary](vocabulary.md): above all, a **session** is
the supervised confined process the daemon owns, and a **conversation** is the
chat thread an agent answers.

## The log/projection split

Two layers, with a strict direction of truth:

| Layer                | Lives in       | Role                                                               | Disposable? |
| -------------------- | -------------- | ------------------------------------------------------------------ | ----------- |
| JSONL event log      | `agentd-events` | Append-only CloudEvents; the source of truth; never rewritten       | No          |
| SQLite projection    | `agentd-node`  | State derived by replaying the log; a read model with a checkpoint  | Yes         |

The SQLite file is **not** a write-ahead log and the JSONL log is **not** a
transient journal. A WAL is a database-internal, truncatable structure; the
JSONL log is permanent. If the projection's schema or reducer
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
    D[("events.jsonl<br/>the log (truth)")] -- "replay + live" --> S["agentd<br/>WebSocket /events"]
    S -- "WireEnvelope { seq, event }" --> C["WsClient"]
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
  --socket "$XDG_RUNTIME_DIR/agentd/agentd.sock" \
  --db /path/to/session/node.db \
  --token-file /path/to/node.token \
  --source urn:mokmokd:node:1 \
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
and it *reacts*: when the conversation's tail is an unanswered turn, it asks the
daemon for a completion on `/inference`, runs the `shell` tool for any tool call,
and publishes the finalized messages as `agent.*` events (see
[inference](inference.md)).

A **conversation** is identified by its id. On start the agent publishes
`agent.conversation.started` (the conversation, workdir, and model), so
conversations are discoverable and auditable from the log. Without
`--conversation`, a new conversation gets a fresh id; with `--resume` the agent
continues the most recent conversation recorded for `--workdir`, which also makes
it pick up a turn interrupted by a restart. A workdir with no recorded
conversation starts a new one instead of failing (the fallback is warned about on
the agent's own stdout, which the session manager that launched it does not
surface — see [Sandbox deployment](#sandbox-deployment)), so the manager can pass
`--resume` unconditionally. `agentd-publish --inbox "..."` sends a prompt to that
same conversation.

```sh
# start a new conversation (paths default to the XDG directories)
agentd-agent --workdir /path/to/workspace --model <alias>

# continue the last conversation for the workspace, then prompt it
agentd-agent --resume --workdir /path/to/workspace --model <alias>
agentd-publish --workdir /path/to/workspace --inbox "fix the failing test"
```

`--model` names a model the daemon resolves (an alias or `provider/model`); when
omitted, the daemon's `default_model` is used. See
[inference](inference.md#providers-and-model-routing).

The agent's token carries `read`, `publish`, and `infer` — never `authority`,
so it cannot forge daemon-authority events. It holds no provider credentials:
the daemon performs inference. Its `agent.*` types are non-reserved, so it can
publish them with the `publish` claim alone.

## Sandbox deployment

Nodes run inside the sandbox. Everything external is a CLI argument
(`--socket`, `--db`, `--token-file`, `--workdir`); the node assumes no host paths
and does not read the environment for config. Its stdout and stderr carry
diagnostics, not state.

The session manager pipes the child's stdio, and except under
`--session-protocol` (where stdout carries protocol frames) it never drains those
pipes: the child's diagnostics are therefore **not** surfaced by the `agentd up`
that launched it (only its `CloudEvents` output over the socket is seen). To read
them, run `agentd-agent` directly; its own diagnostics are on by default, and
`RUST_LOG` narrows or widens them.

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
socket (under `$XDG_RUNTIME_DIR`, or `$TMPDIR`) resolves to the scratch path,
so `--session-command`
must pass the daemon's absolute `--socket`, `--token-file`, and `--db`. SQLite
writes its journal sidecar next to the database, so the whole **directory** must
be writable: place the database in one session `write` entry.

## Stated gaps

- **Synchronous SQLite.** A reducer's transaction runs on the async task that
  receives the event, so a slow disk blocks the node's read loop. There is one
  commit (and its `fsync`) per event. Batching commits and offloading them to a
  dedicated thread are deliberate follow-ups, not done yet because the node's
  only work is this stream and correctness is easier to see with one event per
  transaction.
- **A projection ahead of the log is fatal.** If the daemon starts with a fresh
  log, the node's checkpoint can be beyond the tail. The daemon answers
  `error.resume_out_of_range` and the node stops with
  `NodeError::ResumeOutOfRange`; rebuild with `--rebuild` rather than silently
  resetting state.
- **Changing the filter does not reproject.** Events skipped under an earlier
  filter are not revisited; rebuild the projection to apply a new filter.
- **A log written before a reducer rename does not reproject.** The agent's
  reducer matches `agent.conversation.started`, so a rebuild over a log whose
  history records the event's earlier name leaves the conversation table empty,
  and the projection's checkpoint means an existing database stays empty too.
  Start a fresh log, or name the conversation with `--conversation <id>`; the
  events themselves are untouched either way.

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