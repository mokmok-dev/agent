---
type: Design
title: sandbox
description: Kernel-enforced filesystem and network confinement for agent commands, with a Unix-domain-socket HTTP proxy that makes egress permissions mutable while the sandbox runs
tags:
  - sandbox
  - bubblewrap
  - landlock
  - seatbelt
  - network
  - proxy
  - unix-domain-socket
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## About

This document set specifies the sandbox that confines the filesystem and
network reach of the commands an agent runs, for the 24/7 agent described in
[vision.md](../vision.md).

A command runs under a `Policy` that is rendered into the operating system's
native isolation mechanism. Filesystem access is confined by a path policy
enforced by bubblewrap, Landlock, or Seatbelt. Network egress is denied by
default and, when granted, is routed through a single `HTTP_PROXY` that the
daemon serves over a Unix domain socket (UDS). Because the operating system only
grants the transport to that proxy, the set of reachable destinations lives in
the proxy, not in the kernel policy, and can therefore be **changed while the
sandbox runs**.

## Topics

| Document | Topic |
| --- | --- |
| [filesystem.md](./filesystem.md) | The path policy and its bubblewrap / Landlock / Seatbelt rendering. |
| [network.md](./network.md) | Egress denial by default, the UDS `HTTP_PROXY`, the forwarder, and transport selection. |
| [permissions.md](./permissions.md) | Runtime permission changes: the mutable egress allowlist and per-connection approval. |
| [events.md](./events.md) | The CloudEvents written to the event bus for decisions and violations. |
| [security.md](./security.md) | Trust boundaries, guarantees, and stated gaps. |

## Context and Goals

A continuously running agent executes shell commands and drives third-party
tooling on behalf of a model that may be wrong or manipulated. The workspace it
acts on must be readable and writable enough to do useful work, and nothing else
should be reachable. The boundary must hold against a process the agent spawns,
not merely against the agent's own intent, so it is placed in the kernel rather
than in an in-process check.

The network is the harder half. A command may need to reach a model provider or
a tool service, yet the set of destinations that is safe to reach changes over
time: a task begins with a narrow set, an operator or an approver widens it, and
a destination may be revoked. Kernel network policy cannot express this. Seatbelt
freezes the allowlist into a profile at `exec`; Linux Landlock network rules are
per-port with no host dimension; bubblewrap drops the network namespace
entirely. A design that asks the kernel to allow specific hosts is therefore
either unexpressible or unchangeable at runtime.

Goals:

- Confine filesystem access by a `Policy`, enforced by the kernel, with `deny >
  write > read` precedence and no writes outside an explicit grant.
- Deny all network egress by default; the zero value of a `Policy` reaches
  nothing.
- Route any granted egress through one `HTTP_PROXY` so there is exactly one
  enforcement point for destinations.
- **Change egress permissions while a command runs**, without restarting it:
  add and revoke allowlist entries, and put an unlisted destination to an
  approver per connection.
- Record every decision and every violation as a durable event on the
  [event bus](../event-bus/README.md), so approval and audit share one
  choreography model.

Non-goals:

- A memory or CPU ceiling that the kernel does not enforce. A field with no
  enforcement is a false promise; see [security.md](./security.md).
- A boundary against hostile native code. The sandbox confines the tool calls an
  agent *requests* against a configured policy; it does not make arbitrary
  native code safe.
- An in-process virtual filesystem, a command allowlist, or glob-based path
  rules. Each was considered and deleted because it did not constrain a spawned
  process or could not be enforced; see [filesystem.md](./filesystem.md).
- Cross-host transport of the sandbox control plane beyond what the
  [event bus](../event-bus/transport.md) already specifies.

## Requirements

Functional:

- Render a `Policy` into the platform's isolation mechanism and spawn a command
  inside it.
- Confine filesystem access to the `Policy`'s path entries, with a `deny` inside
  a broader `write` root holding.
- Deny network egress unless the policy opts in.
- Present a granted command with an `HTTP_PROXY` that reaches a daemon-run
  CONNECT proxy over a UDS.
- Enforce a destination allowlist in the proxy, mutable at runtime.
- Ask an approver for an unlisted destination and honour
  grant / deny / cancel.
- Emit a `requested` and exactly one terminal decision event per permission
  check, correlated by `request_id`, plus a structured `violation` event for each
  OS-enforced denial.

Non-functional:

- Fail closed: any setup error before `exec` prevents the command from running
  unconfined.
- The kernel is the boundary; in-process path checks are defense in depth, never
  the guarantee.
- A denied path is not readable, and a denied destination is not reachable by
  any route the sandbox leaves open.
- The sandbox never holds provider credentials; a tunnel is opaque and TLS stays
  end to end.
- Egress permission changes take effect for new connections immediately, and a
  revoke closes established tunnels.

## Assumptions

- Commands run as the daemon's user. The sandbox separates the *agent's* reach
  from the *user's* reach; it is not a privilege boundary between OS users.
- The event bus is the control and audit plane. Approval clients hold an
  **authority** that a confined agent does not; how it is conveyed and verified
  is not yet fixed by the bus and is an [open question](#open-questions), but the
  sandbox's self-approval guarantee depends on it.
- The host provides either bubblewrap or Landlock on Linux, or Seatbelt on
  macOS. A host that provides neither cannot run a confined command and the
  daemon refuses to spawn one rather than run it unconfined.

Unresolved decisions are tracked in [Open Questions](#open-questions).

## Architecture Overview

Two boundaries, and only two, decide what a command can reach: the kernel policy
built at spawn, and the egress proxy the command talks to at run time.

```
   policy · approvals                               event bus
        │  publish                       (UDS + WAL, durable)
        └─────────────────────────────────────► log ──┐
                                                      │ subscribe
   ┌──────────────────── daemon (trusted) ────────────▼─────────────┐
   │   supervisor ──spawn──► kernel policy (static)                 │
   │       │                 bwrap / Landlock / Seatbelt            │
   │       └──── owns ─────► egress proxy (dynamic allowlist)       │
   └──────────────────────────▲─────────────────────────────────────┘
                              │ UDS (crosses the network namespace)
   ┌──────────────────────────┴─────────────────────────────────────┐
   │   sandboxed command (untrusted, private network namespace)     │
   │     bash / agent                                               │
   │       HTTP_PROXY ──► forwarder ──► 127.0.0.1 (in-namespace)    │
   └────────────────────────────────────────────────────────────────┘
```

| Layer | Responsibility | Document |
| --- | --- | --- |
| `policy` | The deny-by-default configuration and its validation. | [filesystem.md](./filesystem.md) |
| `filesystem` | Renders the path policy into bubblewrap / Landlock / Seatbelt. | [filesystem.md](./filesystem.md) |
| `executor` | Spawns a confined one-shot command or long-lived process. | [filesystem.md](./filesystem.md) |
| `egress` | The CONNECT proxy, the child-side forwarder, and the mutable allowlist. | [network.md](./network.md) |
| `permission` | Runtime allowlist mutation and per-connection approval. | [permissions.md](./permissions.md) |
| `events` | Decision and violation CloudEvents on the bus. | [events.md](./events.md) |

The central separation is between the two boundaries:

- The **kernel** decides *whether* the proxy is reachable at all, and whether a
  path is readable or writable. It is set once, at spawn, and does not change.
- The **proxy** decides *which destinations* are reachable through that one
  path. It is consulted per connection and holds mutable state, so it can change
  while the command runs.

Kernel network policy cannot express a mutable host set; the proxy can. Moving
the destination decision into the proxy is what makes runtime permission changes
possible without reopening the boundary.

## Proposed Module Layout

A single crate holds the policy, the filesystem backends, the executor, and the
egress machinery, mirroring the layers above. The sandbox stays
protocol-agnostic: it grants the transport to the proxy and never interprets
what the command sends through it. The daemon serves the proxy process and owns
its lifetime.

```
sandbox/src/
  lib.rs
  policy/         Policy model, validation, path-rule precedence
  filesystem/     bwrap renderer, Landlock spec, Seatbelt profile, backend probe
  executor/       one-shot exec and long-lived spawn
  egress/         CONNECT proxy, mutable allowlist, child-side forwarder
  permission/     approval flow, request correlation
  events/         decision and violation event emission
sandbox/bins/
  sandbox-helper        applies Landlock + seccomp before exec
  egress-forward        bridges loopback to the mounted proxy socket
```

Splitting into separate crates is deferred until a boundary proves stable and
reuse is real.

## Phased Implementation Milestones

1. Policy core: the four domains, validation, and path-rule precedence. Tested
   without spawning anything.
2. Filesystem confinement: the bubblewrap renderer and its namespace probe, the
   Landlock helper with seccomp, and the Seatbelt profile. A confined command
   writes only inside a `write` entry.
3. Executor and events: `exec.completed` and the violation classifier, over the
   event bus.
4. Egress transport: the UDS CONNECT proxy, the child-side forwarder, env
   injection, and Linux fail-closed without bubblewrap.
5. Runtime permissions: the mutable allowlist and the per-connection approval
   flow, correlated by `request_id`.
6. Long-lived processes: spawn, lifecycle events, and supervision.
7. Observability: proxy metrics, verification reporting, and benchmarks for the
   "lighter than a container" claim.

## Open Questions

- The bus gates privileged events on peer identity only: a UDS pathname
  permission plus a `SO_PEERCRED` UID/GID allowlist
  ([event-bus security](../event-bus/security.md)). A confined agent runs as the
  daemon's user and therefore shares that identity, so the **authority** that
  separates an approver from an agent cannot come from the peer UID. It must be a
  token or claim conveyed over the bus (or a second socket with different
  permissions). How is it conveyed and verified, and how does it survive an
  external proxy? See [events.md](./events.md).
- Is the mutable egress allowlist durable across a daemon restart, or is it
  rebuilt by replaying `agent.sandbox.egress.rule_*` events from the log? If it
  is replayed, from which sequence, and how does the rebuild know a later
  `revoke` did not undo an earlier `add`?
- What is the default for a revoked destination with an in-flight tunnel: cut
  immediately (permissions reflect the present) or drain (avoid corrupting a
  request in progress)? The design defaults to cut and records the choice. Is
  the scope per rule or global?
- Should a pattern rule exist (`*.example.com`, host-only any-port), and if so
  what is its precedence against exact rules?
- Should approval be grouped — per connection, per destination per sandbox, or
  per operator-authored session window — so one logical operation does not
  trigger a prompt per connection?
- What approval timeout and rate limit avoid prompt fatigue while keeping a
  human meaningfully in the loop?
- If two authority clients answer the same `request_id`, the first decision wins
  and the rest are ignored. Should a loser's intent be recorded?
- Is the macOS loopback-TCP proxy's host-agnostic gap (any address on the
  granted port) acceptable, or should macOS egress be denied outright until a
  namespace equivalent exists?
- Should a `deny` nested inside a `write` root be a construction error on
  Landlock (which can only grant) or fall back to bubblewrap silently?
