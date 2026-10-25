---
type: Architecture Decision
title: Decisions
description: Each structural choice, the alternative that lost, and the cost we accepted for it.
tags: [adr, decisions]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
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
| 2 | One SQLite database | Framed files in rotated segments |
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
| 16 | Snapshots, so replay does not grow with history | Fold from genesis on every start |
| 17 | `agent-core` is `no_std` with `alloc`, and serialization lives at the edge | A `std` core that parses JSON |

# 1. The log is the only truth

Every derived structure, from the branch tree to the token ledger, is a fold over events. Nothing else may hold state that a fold cannot rebuild.

The rejected design keeps current state in tables and appends an audit row beside each change. It is familiar and fast to query. It also has two write paths that can disagree, and the audit table is usually the one that loses.

Cost accepted. Every query is a fold, so a new view costs code rather than SQL. Mitigated by snapshots and by keeping projections small.

# 2. One SQLite database, not a framed file format

The first draft of this bundle chose length-prefixed frames in rotated segment files. It was reversed before any code was written, and the record stays because the reasoning is what a future reader needs.

What the custom format bought.

- A storage layer a mutation tool can see, since parsing, rotation, and recovery logic would live in Rust where mutants are meaningful.
- A log that is a file, so export is a copy and verification is a scan.

What it cost.

- Every durability property becomes ours to build and to prove. Frame layout and versioning, checksum policy, segment rotation with directory syncs, a tips cache checked against frames, torn-tail truncation, and an archive manifest. Those are the riskiest lines in the system, and the failure mode is silent loss of history.
- Concurrent readers need a protocol we invent.
- Backup needs a consistent copy of an open append-only file, which is a problem in its own right.

SQLite already provides every one of those properties. Crash-safe commits through a write-ahead log, readers that see a consistent snapshot without blocking the writer, a total order, and an online backup that is consistent while the writer runs. What it does not provide is history semantics, and those stay ours. Content hashes, the chain, append-only triggers, the branch DAG, snapshots.

The mutation argument was narrower than it looked. SQL text is opaque to `cargo mutants` under either design, so storage semantics need integration tests regardless. What the gate still covers is the fold, the decisions, and prompt rendering, which is where a silently wrong result is most likely.

Cost accepted. A dependency on SQLite and its Rust binding, a large binary file instead of a readable one, and an export that is a query rather than a copy. In exchange, the code we would have written is code we do not own.

What would reverse it. If the mutation gate turns out to be the only thing catching store defects, or if the log must be readable by a tool that cannot speak SQL. Neither is likely while one process owns the file. The earlier falsifiable trigger never fired, because the code it guarded was never written.

# 3. One writer

A single process appends, enforced by a lock. Readers are unlimited.

The rejected design lets several processes append through a broker, which is how a distributed system begins. It buys throughput we do not need and costs ordering, deduplication, and a second place where history can be lost.

Cost accepted. One process is a restart boundary. The supervisor restarts it in about two seconds, and no accepted event is lost.

# 4. Content hash as id

The id is the hash of the event bytes. Random ids were rejected because a hash makes three problems disappear at once: tamper detection, chain commitment, and duplicate detection on retry.

Cost accepted. `id` changes if any byte changes, including `time`. Building an event twice from the same inputs produces the same id, which is a feature for retries and a surprise for anyone who expects ids to be assigned at write time.

# 5. Canonical JSON on disk

One representation everywhere. The same bytes go into a row, over the socket, and out of an export.

CBOR is smaller and has a deterministic encoding rule. Protobuf is faster to parse. Both add a second representation, and a second representation is a second place for a hash to disagree with a payload.

Cost accepted. JSON is roughly twice the bytes and slower to parse than CBOR. A 30-day history on a personal agent is measured in hundreds of megabytes, which is not a problem worth a codec.

# 6. Branch is a path, not a container

Events are stored once, in one physical log, and a branch is the ancestor chain of a tip.

Per-branch files were rejected because a fork would copy or link history, and both options make the fork either expensive or subtly mutable. A merge-capable DAG was rejected separately, below.

Cost accepted. Reading one branch requires resolving `prev` links, and a branch's events are interleaved with other branches in the file. Both costs are handled by the fold and by snapshots.

# 7. No merge

Merging timelines requires deciding which of two conflicting external effects stands. In an agent runtime, both effects already happened, so no log-based rule can resolve them.

There is a second reason, and it is the stronger one. A prompt is a linear token sequence, and a node with two parents has no canonical linearization. Any merge would have to pick an order, and that order becomes part of every prompt after it, so the same history would render differently depending on the traversal the fold happened to take. The prefix chain would stop being stable, which breaks the one mechanism the branching exists to exploit.

Cost accepted. A winning experiment is promoted forward as new events rather than merged. That loses the ability to keep two branches running and combine them, which no use case in scope needs.

# 8. Tips are derived

The tips cache is checked against the row it points at, and rebuilt on mismatch. The rejected alternative treats a refs file as authoritative, which is how git loses commits after a crash between the object write and the ref update.

Cost accepted. Startup checks each cached tip against its row and rebuilds by query when a check fails. With the cache intact that is one indexed read per branch.

# 9. Surfaces read the store directly

`agentd` is not a query server and not a fan-out broker. Surfaces fold what they need and tail for changes, and the socket carries commands and doorbells.

The rejected design routes reads through the writer. It is simpler for a surface and worse for everyone else, because a slow renderer then holds the writer's attention and a surface cannot answer historical questions at all.

Cost accepted. Every surface links the store crate, so a store bug can crash a surface. That is a crash with no data loss, and the alternative is a bottleneck in the one process that must not stall.

# 10. One process per surface

A surface crash must not stall the reactor, and a leak in a renderer must not take down the writer.

The rejected design puts every surface in one binary. It halves the process count and makes shared state trivial. It also makes a UI panic a full outage.

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

# 16. Snapshots, so replay does not grow with history

The prior art agrees that unbounded history is not a design, it is a deferred incident. Temporal warns at 10,240 events and terminates a workflow at 51,200 events or 50 MB.[^temporal-limits] Restate purges journals after 24 hours by default.[^restate-durable]

This agent runs for years and cannot terminate a conversation, so the bound has to be structural without deleting history. Snapshots are that bound. They do not shrink the database, since no event is ever removed. They bound the work a start has to do, which is the number that would otherwise grow without limit.

Cost accepted. A snapshot format to version and a fold that must stay deterministic, because a snapshot is only useful if the fold that produced it can be repeated and compared. The alternative, folding from genesis on every start, needs no format and makes a cold start proportional to total history, which after a year of running is minutes of CPU before the first command is accepted.

[^langgraph-time-travel]: LangGraph time travel, https://docs.langchain.com/oss/python/langgraph/use-time-travel
[^temporal-limits]: Temporal workflow execution limits, https://docs.temporal.io/workflow-execution/limits
[^restate-durable]: Restate durable execution, https://docs.restate.dev/concepts/durable_execution

# 17. Core is `no_std` with `alloc`, and serialization lives at the edge

The core crate holds the event envelope, the taxonomy, the fold, the decisions, and the canonical encoding. It compiles with `#![no_std]` and `alloc`, and its only dependencies are `serde` and `blake3`.

Two properties follow, and both are the reason for the constraint.

- The core cannot read a clock, open a file, or reach a socket, because the compiler says so. A `std::time::Instant` in a decision function becomes a build error rather than a test that quietly depends on the wall clock.
- Every branch in the core is reachable from data, so the mutation gate is a measurement of the tests rather than of the environment.

Serialization is split deliberately. Core owns the canonical writer, which produces the exact bytes whose hash is the event id. Core never parses those bytes. Decoding a row, and exporting events, happen in the store and the CLI, where `serde_json` lives. The store keeps the canonical bytes verbatim, so verification never re-serializes and there is only ever one encoder in the system.

Cost accepted. `alloc` is required, so this is not bare-metal `no_std`. `HashMap` is out, and `BTreeMap` or a sorted `Vec` stands in, which is an improvement for determinism anyway. Integer formatting, hex, and the RFC 3339 calendar conversions are written here rather than taken from a crate, which is more code to own and more code the mutation gate can measure.

The domain of a timestamp starts at the epoch. A pre-1970 instant from a running service is a broken clock, not history, and excluding it removes a branch from the calendar maths that no test could reach.
