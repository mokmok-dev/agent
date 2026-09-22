---
type: Reference
title: vocabulary
description: agentd のユビキタス言語。名詞を1語1意味で固定し、コード上の実体、イベント型の文法、禁止する別名を定める。
tags:
  - vocabulary
  - language
  - architecture
  - cloudevents
generated:
  by: human
  at: 2026-09-22T00:00:00Z
---

# agentd vocabulary

One word, one meaning. This document is the vocabulary the code and the design
docs are written in: every domain noun lives here with its definition and the
type that embodies it, and the aliases that must not be used are listed at the
end.

**How to use it.** Take a noun from here rather than inventing one beside it. If
a concept has no word yet, add it here in the same change that first uses it. If
a word in this document and a word in the code disagree, the code is wrong or
this document is — either way, one of them changes in that change, never later.

**Exception: quoted vocabulary.** Where a document discusses another
specification or project, that vocabulary stays in its own sense:
`docs/research/*` covers other projects (`session` in ACP, `autonomy level` in
ZeroClaw, and so on), and `docs/reconciliation.md` quotes an external
specification's requirements, including its word "event store". The same applies
where a design doc names a protocol's own concept: an ACP `sessionId` returned by
`session/new` is the child's word for its own session, and is not the daemon's
`session_id`.

## Core — the event world

| Word | Definition | In code |
| --- | --- | --- |
| **Event** | One `CloudEvents` 1.0 envelope: the only durable, addressable unit of change. Its `type` is a dotted name (see [Event types](#event-types)). | `agentd_events::Event` |
| **EventLog** | The append-only JSONL file that is the source of truth, with its writer thread and sequence number. `publish` means "append here"; it returns only after the append is synced. | `agentd_events::EventLog` |
| **Seq** | A one-based position in the log. Positions are derived from line numbers and are never written into the file. | `agentd_events::Seq` |
| **LogEntry** | An Event paired with its Seq, in process. | `agentd_events::LogEntry` |
| **EventBus** | The internal live fanout of committed `LogEntry`s to in-process subscribers. It is `pub(crate)`: an implementation detail of `EventLog`, and not a word to use when describing the system's participants. | `agentd_events::EventBus` |
| **subscriber**, **consumer** | Whoever reads the log: a WebSocket subscriber, a node. The verb is "subscribe"; "listen" and "listener" are not used. | — |
| **Projection** | A read model derived by applying the log in Seq order. Disposable and rebuildable; never a second source of truth. | `agentd_events::Projection`, `catch_up`, `agentd_node::SqliteProjection` |
| **Reducer** | The domain part of a projection: its schema and how one event changes its state. | `agentd_node::SqliteReducer` |
| **checkpoint** | The last Seq a subscriber or projection has applied or skipped. A projection's checkpoint is its `applied_seq`, written in the same transaction as the state it describes. | `Projection::applied_seq` |
| **notice** | A transient wire message that is not part of the log, so its `seq` is `null` (`error.lagged`, `daemon.caught_up`). It must never overwrite a consumer's checkpoint. | `agentd_events::WireEnvelope::notice` |
| **reserved type** | An event `type` only a publisher with the `authority` claim may emit (`daemon.`, `error.`, `sandbox.`, `session.`). | `RESERVED_TYPE_PREFIXES` |
| **authority**, **Claim**, **Principal**, **Token** | The capability model: a connection presents a bearer token, the token maps to a `Principal` with a set of `Claim`s. | `agentd::auth::{Claim, Principal, Token, TokenStore}` |
| **approver** | A client holding `authority` that answers a permission request by publishing a decision carrying the request's `request_id`. | `agentd-approve` |
| **actor** | A participant that publishes and subscribes Events: the daemon, a node, and a `Bridge` on a child's behalf. A child process that speaks only its own protocol is not an actor — its `Bridge` is. | — (design word) |

## Runtime — execution and supervision

| Word | Definition | In code |
| --- | --- | --- |
| **daemon** (`agentd`) | The always-on process: the log, the WebSocket event API, `/inference`, and session supervision. | `agentd` |
| **node** | A long-lived client that subscribes to the log and keeps a projection current. | `agentd_node::Node` |
| **agent** | The node specialization that runs the LLM loop for one conversation. | `agentd_node::Agent` |
| **conversation** | One chat thread: the finalized messages between a user and an agent, identified by its `conversation_id` and scoped by its workdir. What `--resume` continues. | `agentd_node::Conversation`, `--conversation` |
| **Session** | One confined child process the daemon starts and supervises, together with the `Bridge` that gives it a face on the log. Its lifecycle is the `session.*` events. | `agentd::session::SessionManager`, `session.*` |
| **SessionManager** | The component that starts a Session on a `session.requested`, watches its liveness, restarts it within a budget, enforces its lifetime, and reconciles the durable log on startup. | `agentd::session::SessionManager` |
| **supervision** | What the SessionManager does to a Session (restart budget, restart backoff, lifetime). "Supervisor" is a role word for this component in prose, never a type name. | `agentd::session::Supervision` |
| **session_id** | A Session's id, supplied by `session.requested` and used as the event `subject` `session:<id>`. | `session.requested` data |
| **Sandbox** | The handle that runs a command under a `Policy`, and the confinement boundary itself (the OS, not an in-process check). | `agentd_sandbox::Sandbox` |
| **sandbox_id** | The id of one `Sandbox` instance, carried on every sandbox event so concurrent executions correlate. | `sandbox_id` |
| **agent_id** | The identity recorded as the requester of a command in sandbox events (`--session-agent-id`). | `agent_id` |
| **Policy** | The deny-by-default configuration of a Sandbox, in four domains: filesystem, shell, network, limits. Serializable, so it can arrive as an event. | `agentd_sandbox::Policy` |
| **confinement** | The property a `Policy` gives a command: the platform's native isolation (Seatbelt, bubblewrap, Landlock + seccomp), not an in-process check. | — |
| **SandboxedProcess** | A long-lived confined child process with piped stdio, spawned by `Sandbox::spawn`. | `agentd_sandbox::SandboxedProcess` |
| **Executor** | The strategy that runs a command under a policy, one-shot (`exec`) or long-lived (`spawn`). | `agentd_sandbox::Executor` |
| **Protocol** | A factory that creates one `Bridge` per Session for one child protocol. | `agentd::bridge::Protocol` |
| **Bridge** | The per-Session conversion between a child's protocol messages and Events, in both directions. | `agentd::bridge::{Bridge, McpBridge, AcpBridge}` |
| **protocol message** | One message of the child's own protocol (a JSON-RPC object), carried in an event's `data.message`. | `data.message` |
| **frame** | One byte-level unit of that protocol on the child's stdio: a newline-delimited line, capped by `MAX_FRAME_BYTES`. Framing is the `Bridge`'s job. | `MAX_FRAME_BYTES` |
| **turn** | One agent-loop cycle: from an `agent.inbox` until the conversation's tail is answered. A turn may run several tool rounds. | `agent.turn.*` |
| **tool** | Something the agent calls (`shell`, `apply_patch`). A tool call is one request from the model; a tool result is its outcome. | `ToolSpec`, `ToolCall` |
| **inbox** | The user prompt event that starts a turn. | `agent.inbox` |

## Control — permission and violations

| Word | Definition | In code |
| --- | --- | --- |
| **Approval** | The sandbox's approval policy: `Auto` decides by static rule, `Required` waits for an approver or a deadline. | `agentd_sandbox::Approval` |
| **permission request** | `requested` followed by exactly one decision, correlated by `request_id` and stamped with the `sandbox_id` or `session_id` it belongs to. | `sandbox.permission.*`, `session.permission.*` |
| **decision** | The answer to a permission request, with exactly three outcomes: **granted** (allowed), **denied** (refused by a rule or an approver), **cancelled** (nobody decided: a deadline or a withdrawal). A bridged child's grant names the option the approver selected (`option_id`). | `Decision`, `sandbox.permission.granted/denied/cancelled` |
| **Violation** | An operation the OS refused, classified from a command's exit and output: a fact about confinement, not a decision. | `agentd_sandbox::Violation`, `sandbox.violation.*` |

## Network — outside the boundary

| Word | Definition | In code |
| --- | --- | --- |
| **NetworkPolicy** | The network domain of a `Policy`: the Unix sockets a command may reach, whether it has loopback, and the proxy grant. Deny-everything by default. | `Policy::network` |
| **Proxy** | The daemon's CONNECT proxy: the single egress path a confined command may use, and the only place a `host:port` allowlist is enforced. | `agentd::proxy::Proxy` |
| **egress allowlist** | The `host:port` destinations a tunnel may open, plus the approver that may grant an unlisted one. | `agentd::proxy::Egress`, `HostPort` |
| **Forwarder** | The child-side process that bridges a loopback port to the mounted proxy socket, so a child in a private network namespace can still reach the Proxy. It carries no decision. | `agentd_sandbox::forward`, `agentd-egress-forward` |
| **Transport** | How a child reaches the Proxy: a Unix socket (Linux, inside a private network namespace) or a loopback TCP port (macOS, which has no namespace). | `agentd::proxy::Transport` |
| **tunnel** | One accepted CONNECT connection, after which bytes flow opaquely (TLS stays end to end). | — |

## Event types

An event `type` is `<emitter>.<noun>.<verb>`. The noun comes from this document
— never a mechanism (`bridge`) or a protocol (`acp`) — and the verb is one of
`requested`, `granted`, `denied`, `cancelled`, `started`, `exited`, `completed`,
`failed`, `ready`. The emitter is who is accountable for the event, not who
wrote the bytes: the daemon appends and therefore owns `sandbox.*`, `session.*`,
`error.*`, and `daemon.*`.

### Daemon-produced (reserved to `authority`)

| Type | When |
| --- | --- |
| `session.requested` | A client asks the daemon to start a managed session (parameters only — never a command) |
| `session.started` | The session's sandboxed process started |
| `session.restarted` | It restarted after a non-zero exit |
| `session.exited` | It exited |
| `session.failed` | It could not start, or exceeded its lifetime |
| `session.status.requested` / `session.status` | A client asks for the active sessions, and the answer |
| `session.protocol.inbound` / `session.protocol.outbound` | One protocol message from / to a bridged child |
| `session.protocol.ready` / `session.protocol.failed` | The child's protocol handshake completed, or failed |
| `session.prompt.requested` | A client asks a bridged agent to start a prompt turn |
| `session.prompt.completed` | That prompt turn ended, carrying its stop reason |
| `session.permission.requested` | A bridged child asks for permission for a tool call |
| `session.permission.granted` / `.denied` / `.cancelled` | The approver allowed / refused / withdrew it |
| `session.egress.requested` / `.granted` / `.denied` | A destination outside the egress allowlist, and its decision |
| `sandbox.permission.requested` | A command or resource access needs a decision |
| `sandbox.permission.granted` / `.denied` / `.cancelled` | A static rule or an approver allowed / refused it, or nobody decided in time |
| `sandbox.exec.completed` | Terminal state of a one-shot execution (exit code, duration, output sizes) |
| `sandbox.process.started` / `sandbox.process.exited` | A long-lived `SandboxedProcess` was spawned / reached its terminal state |
| `sandbox.violation.filesystem` / `.network` | The OS refused a filesystem / network operation |
| `error.invalid_event` / `.unauthorized` / `.publish_failed` / `.lagged` / `.resume_out_of_range` / `.replay_failed` | A notice: the connection or request could not be served as asked |
| `daemon.caught_up` | A notice marking the end of a resume replay |

### Node-produced (ordinary `publish`)

| Type | When |
| --- | --- |
| `agent.conversation.started` | A conversation began (its id, workdir, and model) |
| `agent.inbox` | A user prompt that starts a turn |
| `agent.message` | A finalized assistant message (with any tool calls) |
| `agent.tool_result` | The result of a tool the assistant called |
| `agent.patch.applied` | A unified diff the assistant applied, with the inverse that undoes it |
| `agent.turn.started` / `.completed` / `.failed` | A turn began / completed / failed |

## Names this document bans

| Banned | Why | Use |
| --- | --- | --- |
| `event store`, `eventstore` | The thing is a log: append-only, ordered, addressed by position. "Store" also reads as the SQLite file. | `EventLog` |
| `store` unqualified | Two different files would share the word. | `EventLog` or `projection` |
| `worker` | As a domain noun there is nothing to name: no pool exists, and each supervised child speaks its own protocol. Reintroduce the word with the pool, not before. (A "worker thread" belongs to a thread pool, not to this vocabulary.) | `Session` (today) |
| `network manager` | There is no such component: the OS enforces the policy, the `Proxy` decides egress, the `Forwarder` carries bytes. | `Proxy`, `NetworkPolicy`, `Forwarder` |
| `mailbox` | An actor-model metaphor for the log; it collides with `protocol message`. | `EventLog` |
| `container`, `jail`, `VM` | The boundary is the platform's own isolation, not a machine. | `Sandbox`, `confinement` |
| `adapter`, `codec`, `driver` as a synonym for `Bridge` | They lose the symmetry of the two directions, collide with the tokio I/O driver, or sound byte-level only. (A provider adapter is a different thing and keeps its name.) | `Bridge` (`McpBridge`, `AcpBridge`) |
| `message` meaning Event | Four concepts already need the word. | `Event`, `notice` |
| `supervisor` as a type | The type is named after what it does to its unit. | `SessionManager` |

## Decisions this document records

- **`session` vs `conversation`.** `session` had three meanings: the supervised
  process, the agent node's chat thread, and a run-loop method. It now means the
  supervised process only; the chat thread is a `conversation`, which is what
  `--conversation` already said. `agent.session.started` became
  `agent.conversation.started`, and the node's session-named identifiers
  (`agent_sessions`, `latest_session`, `session_key`) became conversation-named.
- **The log is not a bus.** Only `EventLog` appends; `EventBus` is its private
  fanout to in-process subscribers. "Publish" therefore means "append to the
  log", and the bus is never the thing a component publishes to.
- **`message` keeps three narrow meanings**: a chat message
  (`agentd_inference::Message`), a protocol message (`data.message`), and a
  transport frame (`tungstenite::Message`). The wire envelope that pairs an
  event with its position is `WireEnvelope`, because docs already called it that.
- **A permission request has three outcomes, not two.** `cancelled` is the
  honest record of a request nobody answered; folding it into `denied` would
  claim the operator refused something they never saw.
- **`worker` is deleted, `actor` is kept** with the definition above. The
  actor-model metaphor stops at `actor`: the log is not a mailbox.
