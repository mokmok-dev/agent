# agent

A control plane for AI coding agents. A person mentions the agent in Slack,
Discord, or Linear. The control plane turns that into a session, chooses the
agent, model, and runtime for it, and runs it either in a Cloudflare sandbox or
on a local VM.

Pi Durable is the conversation: the transcript, the inbox, the retries, and the
crash recovery. Cloudflare's durable Code Mode rules are the execution base:
JavaScript, persistence, replay, and approval. A capability router chooses the
agent, model, and runtime. One runtime port decides whether the agent process
runs in a sandbox or on the VM.

## How it fits together

```
Slack / Discord / Linear
        │  webhook
        ▼
apps/control-plane                Cloudflare Worker
  routes.ts                       verify + normalize
  session-service.ts              route, then record the choice
  session-do.ts                   one Durable Object per conversation
        │
        ├── packages/conversation  pi-durable: the agent's own history
        ├── packages/capability    which agent, model, and runtime
        ├── packages/execution     replay, approval, execution log
        └── packages/runtime       sandbox spawn, VM spawn, supervision
        │
        ▼
agent process                     pi, codex, claude code, opencode, grok build
```

The VM only dials out. It accepts no inbound connection, so nothing on the
internet can reach the agent directly.

## Which agent runs where

| runtime            | agents                                |
| ------------------ | ------------------------------------- |
| cloudflare-sandbox | pi, codex                             |
| local-vm           | pi, claude code, opencode, grok build |

## Packages

| path                    | what it owns                                                    |
| ----------------------- | --------------------------------------------------------------- |
| `packages/protocol`     | JSON-RPC 2.0, the ACP client, and the decoders both sides share |
| `packages/store`        | the session and turn model, its transition table, and the route |
| `packages/adapters`     | Slack, Discord, and Linear signature checks and normalization   |
| `packages/capability`   | the agent catalog and the router                                |
| `packages/conversation` | pi-durable as the conversation and history base                 |
| `packages/execution`    | the durable execution and approval state machine                |
| `packages/runtime`      | the sandbox and VM targets, the supervisor, and line framing    |
| `apps/control-plane`    | the Worker, the Durable Objects, and the local dev server       |
| `apps/gateway`          | the VM bridge, the ACP client, and the outbound relay           |

## Commands

```sh
bun test          # every unit and end to end test
bun run check     # types across the workspace
bun run fmt       # format
bun run mutate    # mutation testing, one gate per package
bun run dev       # local control plane on :8787
```

Every package holds its own mutation gate at 100% killed. A survivor with no
observable effect is recorded in that package's `mutator.accepted.json` with
the reason it is equivalent. `nix flake check` runs the same checks in a
sandbox.

## Local end to end

```sh
bun run dev                                     # control plane, state under .agent-state/
EDGE_URL=ws://localhost:8787/gateway \
AGENT_COMMAND=bun \
AGENT_ARGS=apps/gateway/src/agent/stdio-agent.ts \
  bun run apps/gateway/src/main.ts              # gateway against the scripted agent
```

`apps/gateway/src/e2e.test.ts` drives the same scripted agent through the real
supervisor and bridge, including the permission round trip.

## Turn lifecycle

A turn moves through one table in `packages/store/src/session.ts`:

```
queued ──▶ dispatched ──▶ running ──▶ completed
   │            │            │      └─▶ failed
   │            └─▶ queued   └──────▶ cancelled
   └────────────────────────────────▶ cancelled
```

A delivery is acknowledged with `202` before the agent starts, so a provider
never retries while a turn runs. A replayed delivery returns the turn it
already created, keyed by the provider's event id. A delivery the router
refuses writes no turn, so the provider can retry it.
