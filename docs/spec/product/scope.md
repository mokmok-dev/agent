---
type: Product Scope
title: Agent scope
description: A continuously running personal agent whose entire history is a durable, forkable event log.
tags: [scope, product]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# What this is

A long-lived AI agent that runs on one machine, unattended, for years. It accepts work from a terminal, a browser, and chat platforms. It remembers everything it did as an append-only event log. An operator can rewind that log and branch from an earlier point to retry a turn, a task, or a whole approach with a different model, prompt, or tool policy.

The log is the product. Surfaces are views over it. The model provider is a replaceable dependency.

# Who operates it

One operator runs it on their own hardware, behind their own accounts. Later, a small team shares one instance. Every decision below assumes a single human answerable for the machine, not a multi-tenant service.

# What 24/365 means here

- The agent starts at boot, before login, and restarts after a crash without human action.
- An accepted event survives power loss. The store acknowledges an append only after the bytes are on disk.
- Wrong turns are recoverable by forking, not by hand-editing state.
- A dependency outage degrades capability. It never corrupts history and never spins a restart loop.
- The process fails stopped rather than diverging when it cannot write.

# Non-goals for the first release

- Multi-tenant hosting, per-user quota, or billing.
- Merging two divergent branches into one. Forking is supported, merging is refused by design (see [branching](../architecture/branching.md)).
- A general workflow engine for third-party definitions.
- Running untrusted third-party tool code in-process. Tools are separate processes with declared permissions.
- Distributed consensus across machines. One writer, one machine.

# Design rules

1. Append-only is the truth. Derived state, indexes, and caches are disposable and rebuildable.
2. Every external effect is bracketed by an intent event and a result event, so a crash leaves a record of what might have happened.
3. Prompt prefixes are byte-stable. Volatile content goes at the tail, never at the head.
4. The core is pure. Time, randomness, network, and disk arrive through traits, which is what makes the 100 percent killed mutation gate reachable.
5. A surface never writes to the store. It sends a command and reads events.
6. No silent divergence. If the agent cannot record what it did, it stops doing it.

# How success is measured

Each measure has a definition now and a target once there is a running system to measure. Targets that are not yet measured are marked as proposals in [observability](../architecture/observability.md). No number in this bundle is a measured result unless it cites the run that produced it.

- Replay time for a cold start, measured as wall time to fold 30 days of events.
- Cache read share, measured as cached prompt tokens over total prompt tokens per model call.
- Unrecovered crash count, measured as starts that fail chain verification or find an unreadable database.
- Event loss, which must be zero by construction.
- Mutation score on the logic surface, which must be 100 percent killed.

# Constraints accepted

- One writer per store. Horizontal scale-out is out of scope.
- History is never deleted, so the database only grows. At the expected volume that is under two gigabytes a year, and blobs dominate long before rows do. Retention is an operator decision, documented in [event store](../architecture/event-store.md).
- Provider prompt caches are outside our control. The design makes reuse likely and measures whether it happened. It does not promise a hit.
