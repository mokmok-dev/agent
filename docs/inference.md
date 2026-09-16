---
type: Design
title: inference
description: サンドボックス化されたノードが daemon を介してモデル推論を行う境界の設計。Provider trait、transient な delta ストリーム、認証情報の所在、durable な agent イベントとの線引きを定める。
tags:
  - inference
  - provider
  - agent
  - sandbox
  - eventlog
generated:
  by: human
  at: 2026-09-16T00:00:00Z
---

# agentd inference design

The sandbox denies network egress outright, so a confined agent cannot reach a
model provider directly. Inference is therefore a **daemon capability**: the
daemon holds the provider credentials and performs the request on the agent's
behalf, and the agent reaches it over the daemon's own Unix socket — the one
endpoint its policy grants (see [sandbox](sandbox.md) and
[architecture](architecture.md)). This document is the contract for that
boundary.

## Goals and non-goals

Goals:

- A sandboxed node can request a completion without any network egress and
  without holding provider credentials.
- The provider is swappable behind a trait, so the agent loop can be tested with
  a deterministic fake and a real provider is a thin adapter.
- The volatile token stream never bloats or leaks into the durable audit log;
  only finalized messages become events.

Non-goals:

- **No provider credentials in the sandbox.** The agent's token only grants the
  daemon's inference endpoint; it never sees an API key.
- **No non-daemon remote services.** A directly reached service would need the
  managed-proxy model as a separate opt-in layer (see `docs/sandbox.md`).
- **No inference event log.** Requests and deltas are transient; an
  `inference.*` event family is not reserved or emitted in this version.

## The boundary

Inference is a second WebSocket route on the **same** Unix socket as the event
API, so no policy change is needed and egress stays denied.

```mermaid
flowchart LR
    A["sandboxed agent"] -- "POST completion over UDS" --> D["agentd inference endpoint"]
    D -- "Provider::stream" --> P["provider adapter"]
    P -- "credentials (daemon only)" --> M["model provider"]
    D -- "Delta stream (transient)" --> A
    A -- "finalized messages" --> L["event log (durable)"]
```

| Piece                    | Crate               | Role                                                                 |
| ------------------------ | ------------------- | -------------------------------------------------------------------- |
| Wire contract            | `agentd-inference`  | `Message`, `ToolSpec`, `InferenceRequest`, `Delta`                   |
| `Provider` trait         | `agentd-inference`  | Starts a `Delta` stream for a request; provider-neutral               |
| `FakeProvider`           | `agentd-inference`  | Replays scripted deltas for tests and offline development             |
| `InferenceClient`        | `agentd-inference`  | Node-side client for `/inference`                                     |
| `/inference` endpoint    | `agentd`            | Authenticated endpoint that streams a provider response               |

## Wire contract

A client opens `/inference`, sends one `InferenceRequest` as a text frame, and
reads `Delta` text frames until a terminal delta. The connection stays open for
the next request.

```json
{"messages":[{"role":"system","content":"..."},{"role":"user","content":"..."}],
 "tools":[{"name":"shell","description":"...","parameters":{"type":"object"}}]}
```

The response is one JSON object per frame:

```json
{"type":"text","text":"I will run "}
{"type":"tool_call","id":"call-1","name":"shell","arguments":"{\"command\":\"ls\"}"}
{"type":"done","finish_reason":"tool_calls"}
```

A model-side failure is a terminal `{"type":"error","message":"..."}` delta, not
a transport error, so the agent decides how to record it. A malformed request is
answered with an error delta and the connection stays open.

The shapes mirror the streaming completion protocol most providers expose, so a
provider adapter is mostly a field mapping. `arguments` is kept as the
provider's JSON string, so the contract does not depend on a tool's schema.

## Durability line

| Data                            | Where                | Why                                       |
| ------------------------------- | -------------------- | ----------------------------------------- |
| Request, text/tool deltas       | transient            | high-frequency, large, may be sensitive    |
| Finalized assistant message     | `agent.message` event | the conversation is rebuildable from the log |
| Tool call                       | inside `agent.message` | the call and its reply are one turn      |
| Tool result                     | `agent.tool_result`  | the tool output is part of the record      |

The SQLite conversation projection is rebuilt by replaying the log, so anything
not an event would be lost on rebuild. Only the token stream is transient;
everything the model *decides* is durable.

## Access control

`/inference` requires the `infer` claim (see
[architecture](architecture.md#access-control)). A token without it is answered
with `403 Forbidden`; a missing or unknown token with `401 Unauthorized`. The
agent's token carries `read`, `publish`, and `infer`: it must subscribe to the
log, publish its `agent.*` events, and call inference, but it must **not** hold
`authority`, so it cannot forge `session.*` or `sandbox.*` events.

The provider credentials belong to the daemon and are never passed to the
sandbox. The token file rule (mode `0600`) applies as for any other capability.

## The agent events

The agent publishes these non-reserved `agent.*` types; they are ordinary
publishable events, not daemon-authority ones, because the agent is the author:

| Type                   | When                                              |
| ---------------------- | ------------------------------------------------- |
| `agent.inbox`          | A user prompt that starts a turn                  |
| `agent.message`        | A finalized assistant message (with any tool calls) |
| `agent.tool_result`    | The result of a tool the assistant called         |
| `agent.turn.started`   | A turn began                                      |
| `agent.turn.completed` | A turn completed without a provider error         |
| `agent.turn.failed`    | A turn failed (for example a provider error)      |

`agent.inbox` is the only trigger: an agent runs a turn when an inbox for its
conversation is applied. Every `agent.*` event is projected into the
conversation read model; other events advance the checkpoint unapplied.

## Stated gaps

- **One provider, offline.** The daemon ships the deterministic `FakeProvider`;
  a real provider adapter is the next step and needs a configured credential and
  an HTTP client. The contract is stable, so the adapter is additive.
- **One request at a time per connection.** The connection is
  request/response; concurrent inference uses one connection each.
- **One turn at a time.** The agent runs a turn synchronously, so a prompt that
  arrives during a turn waits; concurrent conversations are separate agents.
- **A timed-out command may leave grandchildren.** The shell tool kills its
  direct child (`bash`) but not the process group, so a backgrounded grandchild
  can outlive the command. It stays confined by the session sandbox; the
  session manager reaps the session's group only when the session ends.
- **A turn is not resumable.** If the agent dies mid-turn, the messages
  finalized before the crash survive in the log, but the in-flight turn is lost.
  The conversation is not corrupted, and a new inbox starts a fresh turn.
- **No provider timeout in the endpoint.** A provider that stalls holds the
  stream open; the real adapter is expected to enforce its own request timeout.
  The request/response contract is additive (a `model` field is a coordinated
  change), so this lands with the first real provider.
- **No token accounting.** The `done` delta may carry a finish reason but usage
  is not yet recorded.
