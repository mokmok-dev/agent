# Architecture

## The four layers

The agent runs in four layers, and each layer owns one question.

| layer        | package                 | the question it answers                                |
| ------------ | ----------------------- | ------------------------------------------------------ |
| conversation | `packages/conversation` | what did the agent say, and what is it doing now?      |
| execution    | `packages/execution`    | may this side effect run, and has it run already?      |
| selection    | `packages/capability`   | which agent, model, and runtime does this session use? |
| runtime      | `packages/runtime`      | where does the agent process run?                      |

Pi Durable is the base of the first layer and Cloudflare's durable Code Mode
rules are the base of the second. Both run on a local VM and inside a
Cloudflare Durable Object, so a session can move between the two without a
second implementation of either.

```
Slack / Discord / Linear
        │  webhook
        ▼
apps/control-plane                Cloudflare Worker
  routes.ts                       verify + normalize (packages/adapters)
  worker.ts                       route to one session object
  session-service.ts              ask the router, then record the choice
  session-do.ts                   one Durable Object per conversation
  thread-index-do.ts              one session id per thread, across races
        │  pi Harness over the object's SQLite, or outbound WebSocket
        ▼
packages/conversation             pi-durable: transcript, inbox, retries
packages/capability               the catalog and the router
packages/execution                replay, approval, execution log
packages/runtime                  sandbox spawn, VM spawn, supervision
        │  stdio JSON-RPC, or an HTTP agent in a container
        ▼
agent process                     pi, codex, claude code, opencode, grok build
```

## What each layer owns

**Conversation.** Pi Durable owns the transcript, the inbox of follow-ups and
steers, retries, and crash recovery. `ConversationBase.open(storage, deps)` is
the only door: it opens pi's `Harness`, returns the root conversation, submits
a prompt keyed by its operation id, and reads the reply from the transcript.
Opening is also resume, because pi recovers what the last process committed.
The control plane stores the pi conversation id on its session and does not
keep a second copy of the agent's words.

**Execution.** `decide`, `recordCall`, `pause`, `approve`, and `reject` are a
pure state machine over an execution record. A resumed pass replays the calls
that already applied, so an approved side effect runs once. Calls are keyed by
their position in the pass, not by name, so a loop that calls one method twice
replays each result correctly. The Cloudflare adapter and the local adapter
supply the executor and the storage; the rules are shared.

**Selection.** The catalog declares one profile per agent and lists the agents
each runtime runs. `route(request, catalog, policy)` honors an explicit choice
or refuses it, and falls back to the policy otherwise. A refusal is data:
`unknown-agent`, `unknown-runtime`, `unsupported-combination`, or
`unavailable-runtime`. The session stores the answer, so an operator can see
what a conversation ran on after the fact.

**Runtime.** One port: a spawn. The local VM target spawns a child of the
gateway; the sandbox target starts a Container on first use and presents one
exec as the same process handle. The supervisor, the line framing, and the
process type live beside the port, so an app imports the seam rather than
owning it.

## Which agent runs where

| runtime            | agents                                | protocol       |
| ------------------ | ------------------------------------- | -------------- |
| cloudflare-sandbox | pi, codex                             | durable, http  |
| local-vm           | pi, claude-code, opencode, grok-build | durable, stdio |

`durable` means the agent loop is a library in the host process, which is what
pi is. `stdio` means a child process speaks a line protocol, which the coding
CLIs do. An agent that a runtime does not list cannot be selected on it at
all, because support is a list rather than a flag.

## Boundaries

Four boundaries carry untrusted data. Each one parses to a typed value before
the rest of the code sees it.

| boundary                           | who parses          | on bad input                                 |
| ---------------------------------- | ------------------- | -------------------------------------------- |
| provider webhook                   | `packages/adapters` | 401 for a bad signature, 400 for a bad body  |
| ACP frame from the agent           | `packages/protocol` | the frame is dropped; the turn keeps running |
| Durable Object storage             | `packages/store`    | a missing session raises `MissingSession`    |
| model output at the agent boundary | pi-durable          | the turn settles unanswered with pi's reason |

Inside those boundaries the code trusts its types. There is no second check of
a value that was already decoded.

## One owner per thing

- A conversation is one `SessionDurableObject`. Cloudflare serializes calls to
  one object, so a delivery never interleaves with another.
- A thread maps to one session id through `ThreadIndexDurableObject`. Two
  webhooks racing for one thread cannot both create a session.
- A session's route is one record, written once. A later delivery reads it
  rather than asking the router again, so a live conversation cannot move to
  another agent.
- The gateway owns the ACP client for its VM. The control plane never speaks
  ACP.
- Pi owns the transcript. The control plane owns the identity and the route.

## Idempotency

Every state change converges when it runs twice.

- A delivery carries the provider's event id. The idempotency key stores the
  turn it created, so a replay is one storage read.
- A prompt carries an operation id. Pi returns the same submission for it, so
  a redelivered prompt is one turn.
- A turn transition derives the next turn from the stored turn. Repeating a
  rejected move changes nothing.
- An approval resumes a paused execution by replay. A second approval changes
  nothing, because no call is pending after the first.

## Failure behaviour

| failure                         | result                                        |
| ------------------------------- | --------------------------------------------- |
| provider retries a webhook      | same turn, no duplicate                       |
| the router refuses a request    | no turn is written, so a retry can route      |
| the agent process crashes       | supervisor restarts it, up to three times     |
| the object is evicted mid-turn  | pi reopens its storage and resumes            |
| the WebSocket drops             | events replay on reconnect                    |
| a permission request arrives    | the turn waits for the person                 |
| a connector call needs approval | the execution pauses, then replays on approve |
| a malformed agent frame         | the frame is dropped, the turn continues      |

## Testing

Every package holds a mutation gate at 100% killed. A survivor with no
observable effect is recorded in that package's `mutator.accepted.json` with
the reason it is equivalent, so the gate stays at 100% and every exception is
reviewed once.

The tests that matter most run the real thing:

- `packages/conversation` drives a real pi-durable turn and a real reopen with
  a scripted Pi AI provider, so the base is proved off Cloudflare.
- `packages/runtime` runs the same frame through a real local child and
  through a sandbox target backed by a real process.
- `apps/gateway/src/e2e.test.ts` runs the real scripted ACP agent over real
  stdio, so the codec, the supervisor, and the bridge are exercised together.

The Durable Object layer cannot be run on this host: `workerd` will not launch
here, and `agents/lifecycle` and `@cloudflare/codemode` need `cloudflare:workers`.
That layer is therefore kept thin, and its logic lives in the tested packages.
