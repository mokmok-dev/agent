---
type: Design
title: supervisor
description: The in-namespace init that runs the forwarder and the confined command together
tags:
  - sandbox
  - supervisor
  - forwarder
  - namespace
  - process
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-28T00:00:00Z
---

## The Supervisor

A confined command that is granted egress needs **two** processes in its network
namespace: the command, and the [`egress-forward`](./network.md#the-child-side-forwarder)
that bridges the command's loopback to the proxy socket. bubblewrap, however,
executes a single process as the namespace's init. Something must start both, and
that something is the supervisor.

```
bwrap --unshare-all ... -- sandbox-supervisor --forward <ef> --socket <s> --port <p> -- <cmd> <args>
                          │
                          ├── fork ──► egress-forward --socket <s> --port <p>   (PID 2)
                          ├── fork ──► <cmd> <args>                             (PID 3)
                          └── wait for <cmd>, then stop the forwarder, exit with its code
```

The supervisor is PID 1 in the namespace. It starts the forwarder, waits until the
forwarder is **listening** (see [Readiness](#readiness)), starts the command, waits
for the command, stops the forwarder, and exits with the command's code. Because
it is PID 1, the kernel reaps orphaned descendants into it, and a single `SIGKILL`
to `bwrap` — which is what the executor's timeout does — brings the whole tree
down with `--die-with-parent`.

## Why a Separate Binary, Not the Forwarder

The forwarder is deliberately dumb: one destination, no decision. Teaching it to
also launch and supervise a command would give the trust-boundary-adjacent bridge
a process-launcher's responsibilities and a second reason to exist. Keeping the
supervisor separate keeps the forwarder a pure byte bridge, and keeps the
supervisor's job — orchestration — in one readable place. See
[network.md](./network.md#the-child-side-forwarder).

## Trust

The supervisor runs **inside** the confinement, as the daemon's user, under the
same policy the command runs under. It is not a security boundary; the kernel
namespace and the proxy are. Two consequences:

- The supervisor must be visible in the command's filesystem view, which the
  broad read grant provides unless a `deny` masks it. The daemon resolves the
  supervisor and the forwarder on the **trusted** side, applying the same rule as
  `bwrap` itself: a binary inside a policy write root is rejected, because a
  repository must not supply the program that confines it. The supervisor is then
  passed the forwarder's already-resolved path, so it never searches.
- A confined command can see and signal the supervisor (same user, same
  namespace). That is acceptable: the supervisor holds no secret. The proxy token
  reaches the command through `HTTP_PROXY` regardless, and the destination
  decision lives in the proxy, not in the supervisor.

## Readiness

The supervisor must not start the command before the forwarder is listening, or an
early `HTTP_PROXY` connection would be refused. It starts the forwarder with its
stderr piped, reads until the forwarder reports its bound port, then starts the
command; the remaining forwarder stderr is drained on its own thread. If the
forwarder exits first, the supervisor treats it as a startup failure and runs
nothing, so a broken bridge fails closed rather than letting the command run with
no route out.

## Lifecycle

| Event | Supervisor action |
| --- | --- |
| Forwarder bound | Start the command. |
| Forwarder exited before binding | Kill it, fail: the bridge is broken. |
| Command exited | Stop the forwarder, exit with the command's code (`128 + signal` if signalled). |
| `bwrap` killed | `--die-with-parent` tears the namespace down; the supervisor does not need to act. |

A long-lived command and a one-shot command take the same path; only the caller's
waiting differs (see [events.md](./events.md#execution-and-process-events)).

## Testing Strategy

- **Arguments**: the CLI accepts `--forward`, `--socket`, `--port`, and the `--`
  separator, and rejects a missing or malformed one.
- **Lifecycle**: the supervisor starts a stub forwarder, waits for its readiness
  line, runs a command that exits non-zero, and returns that code with the
  forwarder stopped.
- **Fail-closed**: a forwarder that exits before reporting readiness makes the
  supervisor fail without running the command.
- **End to end** (later): under one `bwrap`, a command reaches the forwarder's
  loopback port, which carries bytes to the mounted proxy socket.

## Gaps

- **A readiness race remains for a command that connects before it starts.** The
  handshake closes the common case, but a client that connects at the instant the
  forwarder binds can still see a transient refusal; a client that honours
  `HTTP_PROXY` and retries is unaffected.
- **No supervision policy yet.** Restart-on-crash, backoff, and health checks are
  out of scope; the supervisor runs the command once and reports its terminal
  state.
