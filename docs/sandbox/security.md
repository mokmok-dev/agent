---
type: Design
title: security
description: Trust boundaries, guarantees, and stated gaps for the sandbox
tags:
  - sandbox
  - security
  - bubblewrap
  - landlock
  - seatbelt
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Security Model

The sandbox confines the *agent's* reach against the *user's* reach. It is not a
privilege boundary between OS users: a command runs as the daemon's user, and
the boundary is what that user's process may touch.

Trust is arranged in three tiers:

| Tier | Examples | Trust |
| --- | --- | --- |
| Kernel facilities | namespaces, Landlock, Seatbelt | Trusted; they are the boundary. |
| Daemon | supervisor, egress proxy, event bus | Trusted; holds credentials and the allowlist. |
| Sandboxed command | bash, agents, tools | Untrusted; confined by the kernel and routed through the proxy. |

Two properties follow:

- **The kernel is the only filesystem boundary.** Path checks in process are
  defense in depth for a future interpreter, never the guarantee, because a
  spawned interpreter bypasses them.
- **The proxy is the only network boundary.** The kernel grants the transport;
  the proxy decides destinations. A confined command has no other route out.

## Prevented

- **Path traversal and symlink escape** out of a writable root, and replacement
  of the writable root itself, are kernel-enforced on Linux via mount
  namespaces and by the Seatbelt profile on macOS, with symlink rejection at
  construction.
- **Rewriting `.git` or `.agents`** inside a writable root, via
  protected-carveout rules.
- **Host environment leakage**: the environment is an allowlist; the host
  environ is never inherited.
- **Network egress by default**: a confined command cannot open an IP
  connection. The only network reachable is a Unix socket the policy names.
- **Silent host contamination by default**: a write requires an explicit `write`
  entry.
- **Self-approval** (*conditional*): a client cannot publish an
  `agent.sandbox.permission.*` decision or any `agent.sandbox.egress.*` event
  without the authority claim, so an agent cannot approve its own command or
  widen its own reach. This holds only if the bus provides such a claim; today it
  does not, and peer identity cannot supply it because a confined agent shares
  the daemon's UID. See [events.md](./events.md#authority) and
  [README Open Questions](./README.md#open-questions).
- **Reuse of an egress tunnel by another local process**: the proxy requires a
  per-proxy token and answers `407` without it.
- **Runaway loops**: a wall-clock timeout kills the process group.

## Stated Gaps

These are accepted limits, not oversights; each is either unenforceable on the
platform or a deliberate scope choice.

- **No hard memory ceiling.** macOS has no enforcement mechanism and Linux is not
  given a cgroup, so a memory-limit field would be a false promise.
- **The sandbox is not a boundary for hostile native code.** It confines the tool
  calls an agent requests against a configured policy.
- **macOS Seatbelt is unsupported by Apple.** It is functional and widely used,
  but profiles are best-effort and behavior can shift between OS releases.
- **Reads are unconfined unless a `deny` entry names them.** With no denial, a
  spawned host binary can read any file the user can. With egress denied the
  exfiltration channel is closed, but a secret read still reaches the model
  context, so operators should name credentials in `deny` entries.
- **An opted-in egress session weakens the network boundary by design.** It is
  the one case where a confined command can reach a remote host. On Linux the
  tunnel is the only route, so the boundary holds. On macOS the loopback-TCP
  proxy is port-only and host-agnostic: the granted port is reachable on any
  address, and a same-user local process can reach the command's loopback
  server. See [network.md](./network.md).
- **A pre-existing hard link inside a write root aliases a file outside it.**
  The write allowlist matches paths, so the link can be written through.
  Creating it requires access outside the sandbox, so it is a precondition, not
  something a confined command can set up.
- **Linux denials are coarser than macOS.** With bubblewrap a `deny` nested in a
  write root is masked (hidden) rather than carved out read-only, and a fresh
  protected name can still be created inside a write root. With the Landlock
  fallback a nested `deny` cannot be expressed at all, so construction fails
  closed.
- **Landlock network rules need ABI v4 (Linux 6.7).** Older kernels cannot
  enforce port denial, so the fallback fails closed rather than run unfiltered.
- **`AF_UNIX` sockets are not path-scoped by address on Linux.** They are
  governed by the path allowlist; the kernel does not additionally scope the
  connect.

## Fail-Closed Behavior

Every setup step fails closed:

- An `InvalidPolicy` (a `deny` over a needed path, an out-of-root symlink, a
  workdir outside a write entry) rejects the policy before anything runs.
- The Landlock fallback refuses a `deny` it cannot express rather than leave the
  path silently unprotected.
- A host without bubblewrap, Landlock, or Seatbelt refuses to spawn rather than
  run unconfined.
- Linux refuses egress without a private network namespace rather than downgrade
  to the host-agnostic port grant.
- An agent that ignores `HTTP_PROXY` reaches nothing, because the kernel grants
  it nothing else.

## Credentials

- The sandbox never holds provider credentials. A tunnel is **opaque**: the
  proxy does not terminate TLS and never sees the provider key or plaintext.
- The agent holds its own credentials and sends them end to end, as it would on
  an unconfined host.
- The proxy token is per proxy and dies with it; the proxy URL is kept out of
  logs.

## Testing Strategy

- **Escape**: `../`, a symlink out of a root, writable-root rename, an
  environment leak, and a `deny` withholding a secret from a real spawned
  process.
- **Egress**: a confined command cannot open an IP connection; a listed
  destination tunnels and an unlisted one does not; a missing credential is
  `407`.
- **Fail-closed**: a nested `deny` rejects the Landlock fallback; Linux without
  bubblewrap refuses egress; a host with no backend refuses to spawn.
- **Self-approval**: a client without the authority claim cannot publish a
  decision or a rule event.
