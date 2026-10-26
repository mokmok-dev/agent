---
type: Data Model
title: Fold
description: State as a pure function of an event slice, the facts it keeps, and the contradictions it refuses.
tags: [fold, state, determinism]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-26T01:02:00Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-26T01:02:00Z }
---

# What the fold keeps

The fold turns an ordered event slice into state. It keeps facts, not control flow, so a wrong decision cannot be mistaken for a wrong history.

| Fact | Shape | Written by |
|---|---|---|
| Transcript | Role, content, token count, and the turn that caused it, in order | `message.appended`, and the asking projected from `turn.requested` |
| Turns | Turn id and the input that requested it | `turn.requested` |
| Unresolved calls | Call id and tool name, until a result or failure arrives | `tool.intent`, cleared by `tool.result` and `tool.failed` |
| Cache totals | Cached and uncached tokens, and how many observations were reported | `model.cache.observed` |
| Failures | A count of model failures on the branch | `model.failed` |
| Branch facts | Parent, fork point, label, archived flag, depth, tip | The branch events |

The branch tree is one map from branch id to these facts. A branch exists only after its `branch.created` event, so an event on a branch that was never created is a contradiction rather than an empty start.

# What the fold refuses

The fold rejects a contradiction and leaves the state untouched. It never repairs, and it never guesses.

| Contradiction | Why it is not recoverable |
|---|---|
| Event on an unknown branch | The state has nowhere to put it, and inventing a branch would hide a lost creation |
| Branch created twice | One branch, one creation event |
| Predecessor is not the tip | The chain is broken, so the order of facts is unknown |
| Depth skips a number | A gap means an event was lost between the two |
| Same turn requested twice | A retry is a fork, not a second request on the same branch |
| Model event for an unknown turn | The turn was never requested here |
| Result without an intent | The effect has no record of being asked for |

An event type this build does not understand is not a contradiction. It folds to nothing and stays in the log, which is how a newer build's events survive an older one.

# What the fold does not do

- No control flow. Which turn to run, whether to retry, which branch to schedule: those are decisions, and they are not recorded as state.
- No time. A decision that needs the clock receives it as an argument, so a fold cannot depend on when it ran.
- No hash yet. Determinism is checked by folding the same slice twice and comparing the states, which needs no hash. A state hash arrives with snapshots, where something has to compare a stored snapshot with a re-fold and a hash is the cheap way to do it.
- No persistence. The fold reads events and writes nothing.

# Why it is this small

Every field here is either shown to a person or fed to a prompt. A field that is neither is a cache of a decision, and a cache of a decision is how state and history start to disagree.

The cost is that a surface asking a question nobody anticipated pays a fold rather than a lookup. That is the trade recorded in [decisions](decisions.md), and snapshots are what keep the cost bounded as history grows.
