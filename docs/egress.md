---
type: Design
title: egress
description: サンドボックスから外部ネットワークへ到達するための managed proxy 設計。Linux は private netns + bind-mount した Unix socket 上の proxy で経路を一本化し、child 側 forwarder が loopback を橋渡しする。proxy が host:port allowlist を強制する。プラットフォーム別の表現、信頼境界、未解決ギャップを定める。
tags:
  - egress
  - network
  - proxy
  - sandbox
  - seatbelt
  - landlock
generated:
  by: human
  at: 2026-09-19T00:00:00Z
---

# agentd egress design

The sandbox denies all IP egress today, and that is correct for the shell tool:
the daemon mediates inference over its own Unix socket, so a confined command
never needs the network (see [inference](inference.md)). An ACP agent is
different: it speaks its provider's API and cannot be pointed at `/inference`
unchanged, so a supervised ACP agent that cannot reach the network cannot think
(see [acp](acp.md)). This document is the design for giving it a *narrow* path
out without reopening the boundary.

## Why not a host:port allowlist in the OS

The obvious fix — teach `NetworkPolicy` an egress allowlist of `host:port` — is
not expressible on Linux (verified against the current backends):

- **macOS Seatbelt** can render `(allow network-outbound (remote tcp "host:port"))`,
  but the repo already found Seatbelt's socket filters unreliable (`literal` and
  `subpath` silently fail for Unix sockets), and DNS resolution runs through
  `mDNSResponder`'s Unix socket, not port 53.
- **Linux bubblewrap** drops the network namespace with `--unshare-all`; it has
  no per-host filter at all.
- **Linux Landlock** network rules are **per port only** (`NetPort`, `BindTcp`
  or `ConnectTcp`), with no host dimension. A `host:port` allowlist cannot be
  enforced.

So the OS cannot be the thing that decides *which hosts* are reachable. It can
only decide *whether a port is reachable at all*.

## The managed proxy

Invert the problem: the OS grants the child a single destination — a proxy the
daemon runs — and the proxy enforces the host allowlist. The child reaches the
proxy and the proxy opens the real connection only to an allowed `host:port` and
tunnels bytes with HTTP `CONNECT`.

The proxy is reached over a **Unix socket**, not loopback TCP. A Unix socket is a
filesystem object, so it crosses a network namespace: the daemon binds it on the
host and the child gets it bind-mounted in. That matters because it lets the
child run in a **private network namespace** (`bwrap --unshare-all`) with *no IP
egress at all* and still reach the proxy. Inside the namespace the child runs a
tiny **forwarder** that listens on loopback and pipes to the Unix socket; the
agent's `HTTP_PROXY` points at that loopback port.

```
+--------- child (private network namespace) -----------+
|  ACP agent                                            |
|    |  loopback TCP bind (its own HTTP server)         |
|    |  HTTP_PROXY -> 127.0.0.1:FORWARD                 |
|    v                                                  |
|  forwarder (dumb: loopback TCP -> Unix socket)        |
+----|--------------------------------------------------+
     |  Unix socket (bind-mounted; crosses the netns)
     v
+---------------- daemon (trusted) ----------------------+
|  CONNECT proxy  -- allowlist(host:port) --> provider  |
+-------------------------------------------------------+
```

Why the Unix socket, and not loopback TCP, is the whole point:

- **No IP route exists in the namespace.** The child cannot bypass the proxy to
  an arbitrary host, cannot reach UDP/QUIC (there is no external interface), and
  the proxy's address is not a host-reachable port. The earlier "shared network +
  Landlock port filter" model had all three holes; this model has none, and it
  needs no veth (unprivileged bubblewrap cannot make one) and no userspace TCP
  stack.
- **The forwarder is dumb.** It knows one destination (the mounted socket) and
  carries no allowlist or decision. The trust boundary stays on the daemon side,
  where the proxy decides.

What each layer decides:

| Layer | Decides | Enforced by |
| --- | --- | --- |
| Network namespace | loopback only; no external route, TCP or UDP | bubblewrap `--unshare-all` |
| Filesystem profile | the child sees exactly one proxy socket | Landlock / Seatbelt bind-mount |
| Proxy | which `host:port` the tunnel may open | the daemon |
| Provider | authentication | the agent (the tunnel does not see it) |

The tunnel is **opaque**: the proxy does not terminate TLS and never sees the
provider credentials, which the agent holds and sends end to end. That is the
simpler and safer choice; a credential-injecting proxy would need a MITM CA the
agent must trust, and is a separate layer.

The loopback TCP form remains for hosts without a private namespace (macOS
Seatbelt), where the child reaches the proxy on `127.0.0.1`. The Unix-socket
form is the Linux model, because only Linux has the namespace to make it a real
boundary.

## The two mechanisms

A command may need the network for one of two reasons:

- **Loopback only.** An ACP agent starts its own HTTP server on an *ephemeral*
  port and connects to it. It never needs a remote host. A **private network
  namespace** expresses "loopback, and nothing else": inside it `127.0.0.1` is
  the command's own, and every other address is unreachable (`Network is
  unreachable`). bubblewrap's `--unshare-all` provides exactly this.
- **Remote egress.** A command reaches a provider through the daemon's CONNECT
  proxy, which enforces the `host:port` allowlist.

A **networked ACP agent needs both** — its own loopback server *and* the proxy.
The two mechanisms compose on Linux because a **Unix socket crosses a network
namespace**: the daemon binds the proxy on a Unix socket, the child gets it
bind-mounted in, and a small **forwarder** inside the namespace presents it on
`127.0.0.1`. So the child still runs in a private namespace, with no IP route at
all, and reaches the proxy anyway.

That is stronger than a port filter: there is no ephemeral-range hole, no UDP or
QUIC bypass (there is no external interface to send from), and the proxy port is
not a host-reachable address. macOS has no network namespace, so it uses the
weaker loopback-TCP form (below).

## Policy model

```rust
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to (unchanged).
    pub unix_sockets: Vec<PathBuf>,
    /// Free loopback: the command may bind its own server and connect to it on
    /// any ephemeral port. On Linux this lives inside the private namespace.
    pub loopback: bool,
    /// The daemon proxy the command may connect to. `None` grants no egress.
    pub proxy: Option<Proxy>,
}

pub struct ProxyGrant {
    /// The loopback port the child's `HTTP_PROXY` names (the forwarder on
    /// Linux, the proxy itself on macOS).
    pub port: u16,
    /// The proxy's Unix socket (Linux). When set, the child reaches it through
    /// the forwarder; when `None`, the proxy is loopback TCP (macOS).
    pub socket: Option<PathBuf>,
    /// The `host:port` destinations the static allowlist names. With an
    /// approver configured this is the pre-approved subset, not a closed set.
    pub egress: Vec<HostPort>,
}

pub struct HostPort {
    pub host: String,
    pub port: u16,
}
```

The zero value still grants nothing.

## Platform rendering

| Model | macOS Seatbelt | Linux |
| --- | --- | --- |
| loopback | `(allow network-bind/local ip "localhost:*")` + outbound | bubblewrap `--unshare-all`: a private namespace where only loopback exists |
| proxy (Unix socket) | — | private namespace + the mounted socket + `agentd-egress-forward` |
| proxy (loopback TCP) | `(allow network-outbound (remote ip "localhost:P"))` | the Landlock helper: `NetPort(P, ConnectTcp)` |
| unix sockets | path-scoped `remote unix-socket` (existing) | unaffected (filesystem objects) |

Backend selection:

- A **private namespace** is needed for `loopback` alone and for a Unix-socket
  proxy; it requires **bubblewrap**, and construction **fails closed** without it.
- A **loopback TCP proxy** (macOS-style, no namespace) requires the **Landlock
  helper**; construction **fails closed** without it.
- neither prefers bubblewrap, falling back to Landlock for the filesystem and
  seccomp.

The two are the same host property from opposite ends: the daemon picks the
transport (`Proxy::start_for_policy`) from whether bubblewrap is available, and
the executor then selects the backend from the policy shape that choice produced,
so the two cannot disagree.

Notes and gaps:

- **The child-side forwarder is dumb.** It bridges one loopback port to the one
  mounted socket and carries no allowlist or decision; the proxy on the other
  end of the socket is the trust boundary. See `agentd-sandbox::forward`.
- **A Unix-socket proxy needs `agentd-egress-forward`.** It is resolved as a
  sibling of the running daemon (installed next to it), then on `PATH`, then via
  the `AGENTD_EGRESS_FORWARD` override. Every candidate follows the same trust
  rule as the helper: a binary inside a policy write root is rejected. A NixOS
  package installs the daemon, the helper, and the forwarder in one `bin/`, so no
  `PATH` entry is required.
- **The loopback-TCP form is weaker and only for hosts without a namespace.**
  Landlock is port-only and host-agnostic, so the granted port is reachable on
  any address; that form is what Docker Sandboxes' host proxy avoids with a
  microVM. The Linux Unix-socket form has no such gap, which is why Linux uses
  it.
- **Landlock network rules need ABI v4 (Linux 6.7).** On an older kernel the
  crate would drop the handling silently and the `NetPort` rules would become
  no-ops, so a loopback-TCP session would run **unfiltered**. The helper requests
  the net access as a `CompatLevel::HardRequirement`, so a kernel that cannot
  enforce it fails `handle_access` before the command runs.
- **`NO_PROXY` keeps the agent's own loopback off the tunnel.** The daemon
  injects `NO_PROXY=127.0.0.1,localhost,::1` with the proxy env; without it an
  agent routes its internal server calls through the proxy and its session setup
  fails. Verified: an empty `NO_PROXY` breaks `opencode2 acp`'s `session/new`.
- **A networked agent verified end to end.** `opencode2 acp` under
  `--sandbox --egress openrouter.ai:443` (in `agentd/examples/acp_handshake.rs`)
  completes its handshake and a prompt turn **inside a private network namespace
  with no IP route** — external connects fail with `Network is unreachable`,
  while the forwarder's loopback port reaches the mounted socket. `--sandbox`
  alone (loopback, no egress) also works, under `bwrap --unshare-all`.
- **DNS.** A hostname in the allowlist is resolved by the proxy, on the trusted
  side, so the child never needs the resolver. A child that resolves names
  itself still cannot: it has no route except the proxy.
- **TLS stays end to end.** The proxy sees `host:port` and ciphertext, never the
  provider key or the plaintext request.
- **NixOS-style hosts.** The executor resolves its shell from `PATH` (then
  `/bin/bash`, then `/bin/sh`) and grants the shell's directory, and it treats
  `/nix` and `/run/current-system` as system roots, so a host whose interpreter
  lives in a store works. The resolved shell is rejected inside a policy write
  root, so a repository cannot supply it.
- **Loopback is a namespace on Linux, a filter on macOS.** On Linux the private
  namespace makes *every* loopback port reachable and no remote host; on macOS
  Seatbelt grants `localhost:*`, which is the closest equivalent. Neither needs
  a port list, so there is no `0`-means-ephemeral ambiguity. A macOS loopback
  server is reachable by any same-user local process and the command can reach
  other local loopback services (no namespace isolates it); the Linux model does
  not have this reachability, which is a reason to prefer Linux for a confined
  agent.

## The proxy

The proxy is a daemon component, not part of `agentd-sandbox`: the sandbox stays
protocol-agnostic and grants only the transport. It is a small HTTP `CONNECT`
server on either a **Unix socket** (Linux) or **loopback TCP** (macOS):

- It reads the `CONNECT host:port` request line, checks `host:port` against the
  session's allowlist, and either opens a TCP connection and pipes bytes both
  ways, or answers `403`.
- The allowlist comes from the session policy, so an operator authors egress in
  one place.
- The daemon starts one proxy per configured session command (it already runs
  one manager per command), so the proxy's allowlist is that command's policy.

The first increment is the **CONNECT tunnel**: no TLS termination, no
credential injection, allowlist only. A provider that needs a key gets it from
the agent's environment or config, as it would on an unconfined host.

The proxy binds loopback and requires a per-session credential: the URL the
daemon injects (`http://agentd:<token>@127.0.0.1:<port>`) carries a fresh token,
and the proxy answers `407` to a client that does not send it as
`Proxy-Authorization`. This stops another process of the same user from reusing
the tunnel. The token is per proxy and dies with it.

**Gap: the agent must honour the proxy.** The daemon injects
`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` (and `NO_PROXY`) into the sandbox
environment, but whether an agent's HTTP client uses them is the agent's choice.
An agent that ignores the proxy env cannot reach the network, because the OS
grants it nothing else — the failure is closed, not open. `opencode` (Bun)
honours them; this was verified by observing its `CONNECT openrouter.ai:443`.

## Testing strategy

- **Policy tests**: the zero value grants nothing; `loopback` and `proxy`
  round-trip through JSON; the combination validates.
- **Rendering tests**: Seatbelt emits the loopback clauses and the proxy connect
  clause; Landlock emits the `NetPort` connect rules for a loopback-TCP proxy; a
  private-namespace grant with no bubblewrap fails closed; a Unix-socket proxy
  renders the socket mount and the forwarder
  (`linux.rs::a_unix_socket_proxy_mounts_the_socket_and_runs_the_forwarder`).
- **Transport tests**: the transport is chosen from the host's namespace
  capability, not the caller (`proxy.rs`, both branches).
- **Forwarder tests**: it bridges loopback to a Unix socket, and parses its CLI.
- **Proxy tests**: an allowed `host:port` tunnels bytes to a local listener; a
  denied host is refused with `403`; a missing or wrong credential is refused
  with `407`; a client half-close still receives the reply; a Unix-socket proxy
  tunnels too.
- **End-to-end**: a real ACP agent completes its handshake and a prompt turn
  inside a private network namespace with no IP route, reaching the model only
  through the mounted socket. The namespace property itself is asserted in
  `agentd-sandbox/tests/egress_flow.rs`: inside one `bwrap --unshare-all`
  namespace an external address is `Network is unreachable` while the
  forwarder's loopback port carries bytes to the mounted socket (it skips where
  no namespace can be built, as the other spawn tests do).

## Approval for unlisted destinations

A static allowlist is authored by the operator before the session runs. When an
agent must reach a host the operator did not pre-author, the proxy can put the
request to an approver instead of denying it:

1. A `CONNECT host:port` not on the allowlist publishes `session.egress.requested`
   with a `request_id`, and the proxy waits.
2. An approver — any client with the `authority` claim — publishes
   `session.egress.granted` / `session.egress.denied` with the same `request_id`.
3. A grant opens the tunnel; a denial, or no decision within
   `--session-egress-approval-secs`, answers `403`.

The `session.*` type is reserved to authority publishers, so an agent cannot
approve its own egress — the same guarantee the sandbox makes for its permission
events (see [sandbox](sandbox.md#permission-and-violation-events)). Because the
tunnel is the *only* egress path (a private network namespace with no route), an
unlisted destination cannot be reached by bypassing the request, so the detection
is complete. Approval is opt-in; without `--session-egress-approval-secs` an
unlisted destination is denied outright, to avoid prompt fatigue.

## Implementation status

Implemented: the `loopback`/`proxy`/`HostPort` policy fields with validation;
the Seatbelt loopback and proxy clauses; the bubblewrap private namespace; the
**Unix-socket proxy transport** (`Proxy::start_unix`) and the **child-side
forwarder** (`agentd-egress-forward`) that bridges loopback to the mounted
socket; the Landlock helper's `NetPort` rules as the fallback for a loopback-TCP
proxy; the model-driven backend selection, which fails closed without the
required binary; the **egress approval flow** (`session.egress.requested` /
`granted` / `denied`, correlated by `request_id`, timeout denies); and the
`--session-egress host:port` / `--session-egress-approval-secs SECS` /
`--session-loopback` flags with the `NO_PROXY` injection.

**Transport selection** is one decision in one place,
`Proxy::start_for_policy`: a host that can give the child a private network
namespace (`agentd_sandbox::private_namespace_available`, i.e. a trusted
bubblewrap) uses the Unix-socket form, and any other host (macOS Seatbelt, no
namespace) uses loopback TCP, which the profile grants by port. The session
manager and the ACP example both call it, so the loopback-TCP form is reachable
in production and not only from tests.

Verified:

- The proxy tunnels bytes to an allowed destination over both transports,
  refuses a denied one, and refuses a missing or wrong credential (`proxy.rs`).
- An unlisted destination is denied with no approver, granted when an approver
  grants it, kept denied when the approver denies it, and a listed destination
  never consults the approver (`proxy.rs`).
- The transport follows the host's namespace capability, on both branches
  (`proxy.rs`).
- In one `bwrap --unshare-all` namespace, external egress is `Network is
  unreachable` while the forwarder's loopback port carries bytes to the mounted
  socket (`forward.rs`, `linux.rs`, and `agentd-sandbox/tests/egress_flow.rs`).
- A confined command completes a `CONNECT` handshake through the mounted socket
  and receives the origin's body, so bytes cross loopback, the forwarder, the
  socket, and the proxy (`agentd-sandbox/tests/egress_flow.rs`).
- A real agent: `agentd/examples/acp_handshake.rs` drives `opencode2 acp` to
  `session.acp.ready` and through a prompt turn, both with `--sandbox` (private
  namespace, no egress) and with `--sandbox --egress openrouter.ai:443` — the
  latter inside the same no-egress namespace, reaching the model only through
  the socket.

Not built: a private namespace with a veth to the host proxy (unprivileged
bubblewrap cannot create one; the Unix-socket model removes the need),
TLS-terminating credential injection, and a per-provider base-URL rewrite.

Not verified in this repository: the macOS loopback-TCP path. Its Seatbelt
clause and the transport choice are unit-tested, but no macOS host has run a
confined agent through it here, so the profile's runtime behavior is unproven.