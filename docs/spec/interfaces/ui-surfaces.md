---
type: Interface Spec
title: UI surfaces
description: Every surface is a read model plus a command channel, so a UI can be restarted, replaced, or added without touching history.
tags: [ui, tui, webui, chat, discord]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# One contract for every surface

A surface does four things and nothing else.

1. Opens the store read-only and folds the projection it needs.
2. Tails new segments by watching the newest segment's length and the directory listing.
3. Sends commands over `bus.sock` with a `command_id` the surface generated.
4. Renders state. It never writes, never keeps authoritative state, and never decides anything the reactor should decide.

The `command_id` makes a command idempotent. A double-click, a retried HTTP request, and a chat message delivered twice all collapse to one `command.received` event, because the reactor refuses a `command_id` it has already seen.

# The projection each surface reads

| Surface | Projection | Size |
|---|---|---|
| TUI | Branch tree, current transcript, approvals, runtime status | One branch plus the tree |
| WebUI | Same, rendered as HTML plus an SSE stream | One branch plus the tree |
| Chat | One branch per thread, windowed to the last N messages | One branch |
| CLI | Whatever the command asked for | Narrow by construction |

All four fold with the same code from `agent-core`. A difference in what a surface shows is a difference in which projection it asks for, never in how it folds.

# TUI

`ratatui` and `crossterm`. The layout is a branch tree on the left, the transcript in the middle, and a status bar plus input at the bottom.

Keys that matter.

| Key | Action |
|---|---|
| `enter` | Send the input as a turn |
| `n` | New branch from the current tip |
| `f` | Fork dialog, with a turn picker and a rewind count |
| `tab` | Switch branch, with the tree focused |
| `a` | Approve a pending request |
| `d` | Deny a pending request |
| `c` | Show the cache ledger for the current branch |
| `?` | Key help |

The status bar shows the store tip age, the reactor ping age, the active branch, the model, and the cached token share for the current turn. Those five facts are enough to tell a hang from a slow provider from an expired cache.

Streaming output arrives as non-durable frames on the doorbell socket. If the socket drops, the TUI re-reads the log and shows the completed message instead of a partial one.

# WebUI

`axum`, server-rendered HTML, one small vanilla script for the SSE client. No build step, no framework, no bundle.

| Route | Method | Purpose |
|---|---|---|
| `/` | GET | Branch tree and the most recent branch |
| `/b/{branch}` | GET | Transcript for one branch |
| `/b/{branch}/events` | GET | SSE stream of new events and deltas |
| `/b/{branch}/fork` | POST | Fork at a turn or an event |
| `/b/{branch}/turns` | POST | Send a turn |
| `/approvals/{id}` | POST | Decide an approval |
| `/healthz`, `/readyz` | GET | Health, per [observability](../architecture/observability.md) |
| `/metrics` | GET | Prometheus text format |

Binding rules.

- Default bind is `127.0.0.1:7717`. A non-loopback bind requires a token in the config, and the process refuses to start without one.
- Every mutating route requires the token when one is configured, as a header or a form field.
- The SSE endpoint sends the event id with each frame, so a reconnect resumes from `Last-Event-ID` rather than replaying everything.
- No route mutates the store. Every mutation is a command.

# Chat adapters

Discord is the first adapter. The mapping is fixed.

| Chat concept | Store concept |
|---|---|
| Channel with the bot | A set of branches |
| Thread | One branch, id stored in the branch label |
| User message | One `turn.requested` |
| Bot reply | One streamed message per turn, chunked for the platform limit |
| Attachment | A blob plus a message part that references it |
| Reaction or button on an approval | One `approval.decided` |

Rules the adapter follows.

- A message that arrives twice is deduplicated by the platform message id, mapped to a `command_id`.
- A reply is edited in place while streaming, then finalized once. Rate limits are respected with a token bucket, and a dropped edit is not retried, because the final edit carries the whole message.
- Long output is split on paragraph boundaries, never mid-code-fence.
- The adapter holds no state that cannot be rebuilt. A restart re-reads the branch and resumes editing where the transcript says it is.
- Chat is the least trusted surface. A message from a user who is not on the allowlist becomes an event and is ignored, so the attempt is visible in the audit view.

A second adapter for a different platform is a new crate implementing the same four-step contract. It does not touch the reactor.

# Why surfaces cannot be authoritative

Three failures drove this rule.

- A UI that caches state and writes it back produces a second source of truth, and the two drift after the first restart.
- A UI that renders from a websocket feed cannot answer "what did it do an hour ago" without asking the reactor, which makes the reactor a query engine.
- A UI that decides whether an action is safe centralizes policy in three places.

Reading the log directly fixes all three, and it costs one shared crate.

# Accessibility and copy

Terminal output respects `NO_COLOR` and a 80-column minimum. Web pages use semantic HTML with no client-side rendering requirement, so the transcript is readable with JavaScript disabled. Chat messages never use tables, because the platforms do not render them.
