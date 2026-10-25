---
type: Architecture Decision
title: Decisions
description: Each structural choice, the alternative that lost, and the cost we accepted for it.
tags: [adr, decisions]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
sources:
  - id: langgraph-time-travel
    resource: https://docs.langchain.com/oss/python/langgraph/use-time-travel
    title: LangGraph time travel
  - id: temporal-limits
    resource: https://docs.temporal.io/workflow-execution/limits
    title: Temporal workflow execution limits
  - id: restate-durable
    resource: https://docs.restate.dev/concepts/durable_execution
    title: Restate durable execution
---

# How to read this

Each entry names a decision that is expensive to reverse, the alternative that lost, and the cost we accepted. A decision whose cost has not shown up yet is still listed, because the cost is what a future reader needs.

# Summary

| # | Decision | Alternative rejected |
|---|---|---|
| 1 | The append-only log is the only truth | A relational database as truth, projections written to tables |
| 2 | Files with framed records, not SQLite | SQLite with a WAL |
| 3 | One writer, exclusive lock | Many writers with a broker |
| 4 | Content hash as event id | Random ULID |
| 5 | Canonical JSON (RFC 8785) on disk | CBOR, protobuf |
| 6 | Branch is a path through a single-parent DAG | Per-branch logs, or a merge-capable DAG |
| 7 | No merge | Merge with per-type conflict rules |
| 8 | Tips derived, never stored as truth | A refs file as the source of authority |
| 9 | Surfaces read the store directly | A pub/sub broker as the read path |
| 10 | One process per surface | One process with embedded surfaces |
| 11 | Fail stopped on unwritable storage | Degraded in-memory mode |
| 12 | Provider cache only, no local prompt cache | A local prompt cache in front of the provider |
| 13 | 100 percent killed mutants on the logic surface | Coverage thresholds only |
| 14 | OKF 0.2 for the spec | Plain markdown, or a wiki |
| 15 | Fork appends a new root with a parent pointer | Rewriting or copying history |
| 16 | Segments and snapshots, because every surveyed runtime bounds its history | One unbounded file |

# 1. The log is the only truth

Every derived structure, from the branch tree to the token ledger, is a fold over events. Nothing else may hold state that a fold cannot rebuild.

The rejected design keeps current state in tables and appends an audit row beside each change. It is familiar and fast to query. It also has two write paths that can disagree, and the audit table is usually the one that loses.

Cost accepted. Every query is a fold, so a new view costs code rather than SQL. Mitigated by snapshots and by keeping projections small.

# 2. Files, not SQLite

SQLite would give us durability, torn-write handling, and concurrent readers from a battle-tested library. It lost on three points.

- Verification. A hash-chained frame is verifiable with a scan and exportable by concatenation. SQLite integrity checking is about pages, not about the meaning of a row's history.
- Determinism of tests. WAL checkpoint timing, busy timeouts, and page layout are inputs to behavior we would have to control. Our own frame format has three failure modes, and we test all of them at every byte offset.
- Logic drift. A database invites putting fold logic into SQL, where the mutation gate cannot reach it and where a schema migration becomes a behavior change.

Cost accepted. We own the durability code. That is a real risk, stated in [event store](../architecture/event-store.md), and the mitigation is fault injection rather than optimism.

Falsifiable trigger. Two corruption defects traced to the store within one year means this decision is revisited, with the fault-injection suite ported to the replacement.

# 3. One writer

A single process appends, enforced by a lock. Readers are unlimited.

The rejected design lets several processes append through a broker, which is how a distributed system begins. It buys throughput we do not need and costs ordering, deduplication, and a second place where history can be lost.

Cost accepted. One process is a restart boundary. The supervisor restarts it in about two seconds, and no accepted event is lost.

# 4. Content hash as id

The id is the hash of the event bytes. Random ids were rejected because a hash makes three problems disappear at once: tamper detection, chain commitment, and duplicate detection on retry.

Cost accepted. `id` changes if any byte changes, including `time`. Building an event twice from the same inputs produces the same id, which is a feature for retries and a surprise for anyone who expects ids to be assigned at write time.

# 5. Canonical JSON on disk

One representation everywhere. The same bytes go into a segment, over the socket, and out of an export.

CBOR is smaller and has a deterministic encoding rule. Protobuf is faster to parse. Both add a second representation, and a second representation is a second place for a hash to disagree with a payload.

Cost accepted. JSON is roughly twice the bytes and slower to parse than CBOR. A 30-day history on a personal agent is measured in hundreds of megabytes, which is not a problem worth a codec.

# 6. Branch is a path, not a container

Events are stored once, in one physical log, and a branch is the ancestor chain of a tip.

Per-branch files were rejected because a fork would copy or link a segment, and both options make the fork either expensive or subtly mutable. A merge-capable DAG was rejected separately, below.

Cost accepted. Reading one branch requires resolving `prev` links, and a branch's events are interleaved with other branches in the file. Both costs are handled by the fold and by snapshots.

# 7. No merge

Merging timelines requires deciding which of two conflicting external effects stands. In an agent runtime, both effects already happened, so no log-based rule can resolve them.

Cost accepted. A winning experiment is promoted forward as new events rather than merged. That loses the ability to keep two branches running and combine them, which no use case in scope needs.

# 8. Tips are derived

The tips cache is checked against the frame it points at, and rebuilt on mismatch. The rejected alternative treats a refs file as authoritative, which is how git loses commits after a crash between the object write and the ref update.

Cost accepted. Startup scans backward through segments until it finds consistent tips. With the cache intact this is one frame read per branch.

# 9. Surfaces read the store directly

`agentd` is not a query server and not a fan-out broker. Surfaces fold what they need and tail for changes, and the socket carries commands and doorbells.

The rejected design routes reads through the writer. It is simpler for a surface and worse for everyone else, because a slow renderer then holds the writer's attention and a surface cannot answer historical questions at all.

Cost accepted. Every surface links the store crate, so a store bug can crash a surface. That is a crash with no data loss, and the alternative is a bottleneck in the one process that must not stall.

# 10. One process per surface

A surface crash must not stall the reactor, and a leak in a renderer must not take down the writer.

The rejected design puts every surface in one binary. It halves the process count and makes shared state trivially easy. It also makes a UI panic a full outage.

Cost accepted. More units, more install paths, and one socket protocol to keep stable.

# 11. Fail stopped

When the store cannot be written, the process stops. It never continues from memory.

The rejected design keeps serving from a memory copy and writes when the disk recovers. It produces actions that history does not contain, which is the one failure this architecture exists to prevent.

Cost accepted. Availability is worse during a disk problem. The operator sees a failing unit, which is the correct signal.

# 12. Provider cache only

The prompt is byte-identical to what the provider has seen, so the provider's own prefix cache can hit. A local prompt cache would change the bytes and defeat the match, and would only help a local model.

Cost accepted. Reuse depends on a provider behavior we do not control, so the design measures it instead of assuming it. The ledger is the mitigation. See [prompt cache](../architecture/prompt-cache.md).

# 13. 100 percent killed mutants

Coverage was rejected as the gate because a test that runs a line without asserting anything still counts. Mutation testing at zero missed is the only gate that answers whether a test would notice a wrong line.

Cost accepted. Writing tests to kill every mutant is slower than writing tests to reach every line. On a store that must not lose a fact, it is the cheaper mistake. See [mutation policy](../delivery/mutation-policy.md).

# 14. OKF 0.2 for the spec

This bundle is an OKF 0.2 knowledge bundle. The rejected alternative is plain markdown with a table of contents.

OKF costs a small frontmatter block per concept and buys three things that matter for a spec written by an agent. Every concept declares what it is, so a reader can filter by type. Provenance and freshness are frontmatter fields rather than prose. The index is machine-readable, so `agentctl spec check` and any other consumer can traverse the bundle without a parser per document.

Cost accepted. Frontmatter is duplication when a title repeats the filename, and a reader who does not know OKF sees extra lines at the top of each file. The spec links the format so the next reader can look it up.

# 15. A fork is a new root with a parent pointer

Three durable runtimes solve the same problem the same way. LangGraph forks by appending a checkpoint that carries a `parent_checkpoint_id` and leaves the original thread intact.[^langgraph-time-travel] Temporal resets by copying history up to the reset point into a new execution.[^temporal-limits] Restate's restart-as-new copies a journal prefix into a new invocation id, which is exactly the shape of "same history, different code".[^restate-durable]

All three append rather than rewrite. None of them makes a fork a first-class fact in the log, which is the one thing this design adds, because a first-class fork is what makes the cache ledger answerable per branch.

Cost accepted. The branch tree is a global projection, so a reader of one branch cannot see its children without scanning events from other branches. A copy-on-fork design would avoid the scan and cost a rewrite per fork.

# 16. Segments and snapshots, because every surveyed runtime bounds its history

The prior art agrees that unbounded history is not a design, it is a deferred incident. Temporal warns at 10,240 events and terminates a workflow at 51,200 events or 50 MB.[^temporal-limits] Restate purges journals after 24 hours by default.[^restate-durable]

This agent runs for years and cannot terminate a conversation, so the bound has to be structural. That is why segments rotate at a fixed size and why snapshots let a replay start near the tip instead of at genesis.

Cost accepted. Segmentation and snapshots are two formats to write, test, and version. The alternative, one growing file opened at startup, would need the same recovery logic and would make a cold start proportional to total history.

[^langgraph-time-travel]: LangGraph time travel, https://docs.langchain.com/oss/python/langgraph/use-time-travel
[^temporal-limits]: Temporal workflow execution limits, https://docs.temporal.io/workflow-execution/limits
[^restate-durable]: Restate durable execution, https://docs.restate.dev/concepts/durable_execution
