---
type: Design
title: acp
description: ACP (Agent Client Protocol) 対応の設計。MCP とは逆に子がエージェント本体で client(daemon) が要求を受けるため、共有ステートレス codec ではなくセッション単位のステートフル Bridge、JSON-RPC id 相関、permission の人間承認フロー連携、そして sandbox egress の未解決ギャップを定める。
tags:
  - acp
  - bridge
  - jsonrpc
  - permission
  - sandbox
  - eventlog
generated:
  by: human
  at: 2026-09-19T00:00:00Z
---

# agentd ACP design

The Agent Client Protocol (ACP, <https://agentclientprotocol.com>, v1 stable) is
the JSON-RPC 2.0 protocol Zed uses to talk to coding agents. Running an ACP agent
as a supervised child is the target: the agent is confined by the sandbox and the
daemon is its client. This document fixes what that requires, why the existing
[`Bridge`](session.md) shape is not enough, and the one gap — network egress —
that is not solvable inside the sandbox.

## Why ACP is not just another codec

`McpBridge` is a stateless line codec: `uplink` and `downlink` are pure
functions of one line, and the manager routes by `subject`. ACP breaks both
assumptions.

| | MCP (built) | ACP (this design) |
| --- | --- | --- |
| Child is | a tool server | the agent itself |
| Daemon is | the tool client | the editor client |
| Who calls whom | daemon -> child | both directions |
| Child -> daemon requests | rare | required (`session/request_permission`; `fs/*`, `terminal/*` optional) |
| Ordering | each line independent | `initialize` -> `session/new` -> `session/prompt` |

Three consequences:

1. **Id correlation.** ACP reuses JSON-RPC ids across a bidirectional
   request/response stream. When the child sends `session/request_permission`
   with `id: 5`, the daemon must reply with `id: 5`. A pure codec that ignores
   ids cannot do this.
2. **A state machine.** The handshake is not a fixed list of lines: the client
   sends `initialize`, waits for the response, sends `session/new`, waits for
   `sessionId`, then may send `session/prompt`. A static `handshake() -> Vec` cannot
   express a response-dependent sequence.
3. **A client role.** The daemon must implement the client side of the protocol
   — at minimum the baseline `session/request_permission`, and optionally
   `fs/read_text_file`, `fs/write_text_file`, and the `terminal/*` methods.

## Bridge shape change

The `Bridge` trait becomes session-scoped and stateful. A shared `Protocol`
factory creates one `Bridge` per supervised session; each `Bridge` is a small
state machine that turns lines and events into [`Action`](#actions).

```rust
/// A protocol, shared across sessions. Creates a state machine per session.
pub trait Protocol: Send + Sync {
    fn connect(&self, context: &SessionContext<'_>) -> Box<dyn Bridge>;
}

/// What a protocol needs to know about the session it is connecting.
pub struct SessionContext<'a> {
    pub session_id: &'a str,
    /// The sandbox workdir, advertised as the ACP session `cwd`.
    pub workdir: &'a Path,
}

/// The per-session state machine. `&mut self`, so ids and phase are per session.
pub trait Bridge: Send {
    fn start(&mut self) -> Vec<Action>;
    fn on_event(&mut self, event: &Event) -> Vec<Action>;
    fn on_line(&mut self, line: &str) -> Vec<Action>;
}

/// What the manager should do with the result of one bridge step.
pub enum Action {
    /// Append this event (the manager stamps `subject`).
    Publish(Event),
    /// Write this line to the child.
    Write(String),
}
```

The manager wraps a session's `Bridge` in `Arc<Mutex<Box<dyn Bridge>>>` so the
single-writer stdin task and the uplink reader can both drive it, holding the
lock only for the duration of one synchronous `on_line`/`on_event` call. All I/O
stays in the manager, so this preserves the existing single-writer and
bounded-read guarantees (see [session](session.md)).

`McpBridge` is refactored onto this trait without behavior change: `start()`
returns the `initialize` request, `on_line` returns the `notifications/initialized`
reply plus the event, and `on_event` returns the `tools/call` line. The stateless
codec becomes a state machine whose state happens to be constant.

## AcpBridge

`AcpBridge` implements the client side of ACP v1.

Phase state:

```text
Started -initialize->  Initialized -session/new->  Ready -session/prompt->  Prompting
                            |                              ^                       |
                            +------ (agent request) -------+------- (reply) --------+
```

- `start()` emits the `initialize` request with the client capabilities below
  and `id: 0`.
- On the `initialize` response it emits `session/new` with the session's `cwd`
  and an empty `mcpServers` list, and remembers the ACP `sessionId` when it
  arrives.
- Once `Ready`, a downlink event `session.bridge.prompt` emits `session/prompt`.

Client capabilities advertised: `fs.readTextFile: false`,
`fs.writeTextFile: false`, `terminal: false`, no elicitation. The agent is
expected to do its own filesystem and shell work inside the sandbox, so the
daemon does not proxy them. `session/request_permission` is baseline and must be
answered.

### Request/response correlation

Two id tables: `pending: HashMap<u64, Pending>` for requests the daemon sent
(the daemon is the caller), and `inbound: HashMap<String, Value>` for requests
the agent sent that need an answer (the daemon is the responder). Outbound ids
are monotonically allocated; an inbound permission request's id is captured when
it is turned into an event, so the eventual decision can be returned under the
same id. A response line is matched to its `pending` entry and never treated as
a new request; an inbound request whose method is not one the daemon implements
is answered with a JSON-RPC `Method not found`, so the agent never blocks on an
id.

## Permission, human-in-the-loop

`session/request_permission` is the point where an autonomous agent pauses for a
human. The daemon does not answer it locally with a blanket rule; it reuses the
same durable-decision model as the sandbox approval flow (see
[sandbox](sandbox.md#permission-and-violation-events)):

1. `AcpBridge` receives `session/request_permission` (id `N`), publishes
   `session.permission.requested` carrying the tool call and the offered
   options, and keeps id `N` pending.
2. An approver — any WS client with the `authority` claim — publishes
   `session.permission.decided` with the same correlation id and the chosen
   `optionId` (or `cancelled`).
3. The manager routes that decision to the bridge, which emits the
   `request_permission` response under id `N`.

Because both types carry the reserved `session.*` prefix, only an authority
client can decide, so an agent cannot approve its own tool call — the same
guarantee the sandbox makes for its own permission events.

Event types this adds:

| Type | Direction | Payload | Builder |
| --- | --- | --- | --- |
| `session.bridge.prompt` | downlink | the prompt content blocks to send | `bridge::prompt(blocks)` |
| `session.bridge.inbound` | uplink | one agent JSON-RPC message (existing type) | `bridge::bridged_inbound` |
| `session.permission.requested` | uplink | `request_id`, `tool_call`, `options` | — |
| `session.permission.decided` | downlink | `request_id`, `option_id` or `cancelled` | `bridge::permission_decided` / `permission_cancelled` |
| `session.acp.ready` | uplink | the handshake completed | `bridge::ACP_READY` |
| `session.acp.failed` | uplink | the handshake or a turn failed | `bridge::ACP_FAILED` |
| `session.acp.turn.completed` | uplink | a prompt turn ended, with its stop reason | `bridge::ACP_TURN_COMPLETED` |

## The egress gap (unresolved)

**An ACP coding agent reaches its own model provider over the network, and the
sandbox denies all IP egress.** Unlike inference, which the daemon mediates
over its Unix socket (see [inference](inference.md)), an ACP agent speaks the
provider's own API and cannot be pointed at `/inference` unchanged. A bridge
built on today's sandbox would supervise an agent that cannot think.

A narrow host:port egress grant, the obvious fix, is **not achievable on Linux**
(verified against the current backends):

- **macOS Seatbelt** can render `(allow network-outbound (remote tcp "host:port"))`,
  but the repo already found Seatbelt's socket filters unreliable (`literal`
  and `subpath` silently fail for Unix sockets, `docs/sandbox.md`), and
  host-scoped TCP grants are unproven here. DNS resolution also runs through
  `mDNSResponder`'s Unix socket, not port 53.
- **Linux bubblewrap** drops the network namespace with `--unshare-all`; it has
  no per-host filter. Narrow egress is impossible without giving up the network
  namespace entirely.
- **Linux Landlock** network rules are **per port only** (`NetPort`), with no
  host dimension. A host:port allowlist cannot be enforced.

So the honest options are a blanket reopen (rejected: it discards the boundary)
or a **managed proxy**. The latter is designed in [egress](egress.md): the
daemon runs a CONNECT proxy, the OS grants the proxy's loopback port (and, for an
agent with its own server, the loopback ephemeral range), and the proxy enforces
a `host:port` allowlist. A session opts in with `--session-loopback
--session-egress host:port`. It is an opaque tunnel — no TLS termination and no
credential injection — so the agent still holds its own provider key.

An ACP agent starts and completes its handshake confined, in two ways:

- `--session-loopback` alone: a private network namespace, loopback only, no
  egress. Needs bubblewrap.
- `--session-loopback --session-egress openrouter.ai:443`: the shared network
  with the proxy port and the ephemeral range open, so the agent reaches the
  provider *and* keeps its own server. The daemon injects `NO_PROXY` so the
  agent's own loopback stays off the tunnel. `agentd/examples/acp_handshake.rs`
  drives a real `opencode2 acp` to `session.acp.ready` and through a prompt turn
  this way; the proxy observes the agent's `CONNECT openrouter.ai:443`.

The agent must honour the injected proxy env (see the egress doc's gap);
`opencode` does. A state directory it can write is a separate filesystem
concern.

## Testing strategy

- **Protocol unit tests**: the state machine advances
  `initialize` -> `session/new` -> `Ready` on the right responses; outbound ids
  are monotonic; a response is matched to its `Pending` and not re-emitted as a
  request; foreign/blank/non-JSON lines are ignored.
- **Id-correlation tests**: `session/request_permission` publishes
  `session.permission.requested`; a `session.permission.decided` produces the
  `request_permission` response under the original id.
- **Capability tests**: the advertised `initialize` carries the documented
  client capabilities and protocol version.
- **End-to-end tests**: a scripted child on `/bin/sh` runs the handshake and one
  permission round trip over the log, with everything under
  `subject: session:<id>`.
- **Regression**: `McpBridge`, now on the stateful trait, passes its existing
  tests unchanged.

## Implementation status

Implemented: the stateful `Protocol`/`Bridge`/`Action` traits; the
`McpProtocol`/`McpBridge` refactor onto them; `AcpProtocol`/`AcpBridge` (phase
machine, id correlation, permission round trip, unknown-method error replies);
the `PROMPT`/`session.permission.*`/`session.acp.*` event types with typed
builders; `SessionManager::with_protocol` and `with_workdir`; and the
`--session-bridge acp` flag. All behind the `sandbox` feature.

Not built: `fs/*` and `terminal/*` client methods (capabilities are advertised
false, so the agent does its own work; an unsupported request is answered
`Method not found`), `session/load`/`resume`, and the **egress gap below** — an
ACP agent that reaches a remote model provider cannot run under the sandbox yet.

Verified against a real agent: `agentd/examples/acp_handshake.rs` drives
`opencode acp` through `AcpProtocol` and completes the handshake
(`initialize` -> `session/update` -> `session/new`, then `session.acp.ready`).

It runs both ways: unconfined by default, and `--sandbox` confined. The confined
run works because the agent's internal HTTP server needs free loopback, which is
a private network namespace (see [egress](egress.md#two-network-models)): it
binds an ephemeral port and connects to it with no external route. That needs
bubblewrap; the Landlock fallback cannot express free loopback without opening
egress, so it fails closed. Reaching a remote model remains the proxy model's
job and is the open part below.