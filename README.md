# agent

A control plane for AI coding agents, with the agent running on its own VM.
A person mentions the agent in Slack, Discord, or Linear. The control plane
turns that into a session and a turn, the VM runs one ACP agent per session,
and every update flows back to the thread.

## How it fits together

```
Slack / Discord / Linear
        │  webhook
        ▼
apps/control-plane       Cloudflare Worker
  routes.ts              verify + normalize (delegates to packages/adapters)
  worker.ts              route to one session object
  session-do.ts          one Durable Object per conversation
  thread-index-do.ts     one session id per thread, across races
        │  outbound WebSocket
        ▼
apps/gateway             on the VM
  main.ts                dial out, replay the local event log
  bridge.ts              own the ACP client, attribute updates to a turn
  supervisor.ts          restart a crashed agent up to a limit
        │  stdio JSON-RPC
        ▼
agent process (ACP server)
```

The VM only dials out. It accepts no inbound connection, so nothing on the
internet can reach the agent directly.

## Packages

| path                 | what it owns                                                          |
| -------------------- | --------------------------------------------------------------------- |
| `packages/protocol`  | JSON-RPC 2.0, the ACP client, and the decoders both sides share       |
| `packages/store`     | the session and turn model, its transition table, and the repository  |
| `packages/adapters`  | Slack, Discord, and Linear signature checks and payload normalization |
| `apps/control-plane` | the Worker, the Durable Objects, and the local dev server             |
| `apps/gateway`       | the VM bridge, the process supervisor, and line framing               |

## Commands

```sh
bun test          # every unit and end to end test
bun run check     # types across the workspace
bun run fmt       # format
bun run mutate    # mutation testing; fails on any undetected mutant
bun run dev       # local control plane on :8787
```

Every package holds its own mutation gate at 100% killed. `nix flake check`
runs the same four checks in a sandbox.

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
already created, keyed by the provider's event id.
