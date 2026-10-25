---
type: Glossary
title: Glossary
description: The nouns this spec uses, each with exactly one meaning.
tags: [glossary]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# Terms

**Event.** One immutable record in the store. It is a CloudEvents 1.0 structured-mode message plus the extension attributes. It carries a content hash as its `id`.

**Store.** The state directory holding the database, the blob directory, and the writer lock. Exactly one process appends to it at a time.

**Log.** The total order of rows in the events table, in insert order. A row's physical position is its row id.

**Branch.** A named path through the event DAG. The store holds many branches. A branch is identified by a stable string such as `main` or `b-01JA3`.

**Fork.** The act of creating a branch whose first event's predecessor is an event on another branch. A fork copies no events.

**Depth.** The number of ancestors of an event, counted from the genesis event at depth zero. Depth is derived, never assigned.

**Tip.** The deepest event of a branch. Tips are derived by query, never stored as truth.

**Fold.** A pure function from an ordered event slice to derived state. The same slice always produces the same state, and the state carries a hash so two folds can be compared.

**Projection.** A derived read model built by folding. Examples are the branch tree for the TUI, the transcript for the WebUI, and the token ledger.

**Turn.** One model request and everything it triggers, from the operator's message to the final assistant message. A turn is a unit of retry and a unit of cache accounting.

**Segment (prompt).** One byte-stable slice of a rendered prompt, such as the system message, the tool schema block, or one conversation message. Prompt segments are hashed to form the prefix chain. Unrelated to a database row, which is a stored event.

**Prefix chain.** The ordered list of hashes `h1 = H(s1)`, `h2 = H(h1 || s2)`, and so on. A cache lookup finds the longest matching prefix chain entry.

**Cache ledger.** A projection that records, per model call, the prefix hash sent, the tokens the provider reported as cached, and the time of the call.

**Command.** A request from a surface to the runtime, such as `fork` or `send`. A command becomes an event. A command is never applied directly to state.

**Effect.** Anything outside the process that cannot be undone by replay, such as a shell command, an HTTP POST, or a chat message. Effects are bracketed by intent and result events.

**Reactor.** The single loop in `agentd` that reads new events, advances timers, decides the next action, and appends events.

**Fail stopped.** The store refuses further work and the process exits when it cannot make durable progress. The alternative, continuing in memory, is forbidden.
