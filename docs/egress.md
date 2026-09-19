---
type: Design
title: egress
description: サンドボックスから外部ネットワークへ到達するための managed proxy 設計。sandbox は loopback の proxy ポートだけを許可し、proxy が host:port allowlist を強制する。プラットフォーム別の表現、スレッドモデル、未解決ギャップを定める。
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

## Two network models

A command may need the network for one of two reasons, and they want opposite
mechanisms:

- **Loopback only.** An ACP agent starts its own HTTP server on an *ephemeral*
  port and connects to it. It never needs a remote host. The only safe way to
  express "loopback, and nothing else" is a **private network namespace**:
  inside it `127.0.0.1` is the command's own, and every other address is
  unreachable (`Network is unreachable`). bubblewrap's `--unshare-all` provides
  exactly this.
- **Remote egress.** A command reaches a provider through the daemon's CONNECT
  proxy. This needs the *shared* network with only the proxy port open, which is
  the Landlock helper's `NetPort` filter.

The two are different mechanisms, but not exclusive: a **networked ACP agent
needs both** — its own loopback server *and* the proxy. That combination cannot
use a private namespace (which has no route to the host's proxy), so it takes
the *shared* network with two grants at once:

- the proxy port, and
- the host's ephemeral range, so the agent's own server can bind a random port
  and connect to it.

`loopback` with `proxy` therefore means "shared network, proxy port and ephemeral
range open"; `loopback` alone means "private namespace, loopback only".

## Policy model

```rust
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to (unchanged).
    pub unix_sockets: Vec<PathBuf>,
    /// Free loopback: the command may bind its own server and connect to it on
    /// any ephemeral port. Alone this is a private namespace (no egress); with a
    /// `proxy` it is the shared network with the ephemeral range open.
    pub loopback: bool,
    /// The daemon proxy the command may connect to: its loopback port and the
    /// host:port allowlist the proxy enforces. `None` grants no egress.
    pub proxy: Option<Proxy>,
}

pub struct Proxy {
    pub port: u16,
    /// Hosts the proxy may open a tunnel to. The OS does not enforce this; the
    /// proxy does, which is the whole point.
    pub egress: Vec<HostPort>,
}

pub struct HostPort {
    pub host: String,
    pub port: u16,
}
```

The zero value still grants nothing. `loopback` and `proxy` are the only IP
grants.

## Platform rendering

| Model | macOS Seatbelt | Linux |
| --- | --- | --- |
| loopback alone | `(allow network-bind/local ip "localhost:*")` + `(allow network-outbound (remote ip "localhost:*"))` | bubblewrap `--unshare-all`: a private namespace where only loopback exists |
| proxy | `(allow network-outbound (remote ip "localhost:P"))` | the Landlock helper: `NetPort(P, ConnectTcp)` on the shared network |
| loopback + proxy | the loopback grants plus the proxy grant | shared network: `NetPort(0, BindTcp)` (the ephemeral range) and `NetPort(p, ConnectTcp)` for `p` in the ephemeral range **and** the proxy port |
| unix sockets | path-scoped `remote unix-socket` (existing) | unaffected (filesystem objects) |

Backend selection follows from the model:

- `loopback` **without** a proxy requires **bubblewrap** (the private namespace);
  without it, construction **fails closed**.
- `proxy` (with or without `loopback`) requires the **Landlock helper** (the port
  filter); without it, construction **fails closed**.
- neither prefers bubblewrap and falls back to Landlock for the filesystem and
  seccomp.

Notes and gaps:

- **Landlock has no connect range, so the ephemeral range is enumerated.** The
  agent binds a random ephemeral port and connects to it; Landlock's one range
  form (`port 0`) covers **bind only**, so the connect side is ~28k explicit
  `NetPort` rules read from `/proc/sys/net/ipv4/ip_local_port_range`. The kernel
  accepts them in a few milliseconds; that is why the helper's spec lists ports.
- **The combined model opens the ephemeral range to any host, not just
  loopback.** Landlock has no host dimension, so a connect grant for an
  ephemeral port allows that port on *any* address. An agent (or a command that
  borrows its environment) could reach an arbitrary server on an ephemeral port.
  The proxy port is likewise host-agnostic. This is the price of giving a
  networked agent its own loopback server under a port-only filter; a private
  namespace plus a veth to the host proxy would remove it, and unprivileged
  bubblewrap cannot create one. Stated gap.
- **Landlock network rules need ABI v4 (Linux 6.7).** On an older kernel the
  crate would drop the handling silently and the `NetPort` rules would become
  no-ops, so a networked session would run **unfiltered**. The helper requests
  the net access as a `CompatLevel::HardRequirement`, so a kernel that cannot
  enforce it fails `handle_access` before the command runs.
- **`NO_PROXY` keeps the agent's own loopback off the tunnel.** The daemon
  injects `NO_PROXY=127.0.0.1,localhost,::1` with the proxy env; without it an
  agent routes its internal server calls through the proxy and its session setup
  fails. Verified: an empty `NO_PROXY` breaks `opencode2 acp`'s `session/new`.
- **A networked agent verified end to end.** `opencode2 acp` under
  `--sandbox --egress openrouter.ai:443` (in `agentd/examples/acp_handshake.rs`)
  completes its handshake and a prompt turn, and the proxy observes
  `CONNECT openrouter.ai:443`. `--sandbox` alone (loopback, no egress) also
  works, under `bwrap --unshare-all`.
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
protocol-agnostic and only grants a port. It is a small HTTP `CONNECT` server on
`127.0.0.1:<proxy.port>`:

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
  clause; Landlock emits the `NetPort` connect rules and, for loopback + proxy,
  the ephemeral range and the bind-range rule; a loopback grant with no
  bubblewrap fails closed.
- **Proxy tests**: an allowed `host:port` tunnels bytes to a local listener; a
  denied host is refused with `403`; a missing or wrong credential is refused
  with `407`; a client half-close still receives the reply.
- **End-to-end**: a real ACP agent completes its handshake and a prompt turn both
  confined with no egress and confined with the proxy.

## Implementation status

Implemented: the `loopback`/`proxy`/`HostPort` policy fields with validation
(including the combination); the Seatbelt loopback and proxy clauses; the
bubblewrap private-namespace model for `loopback` alone; the Landlock helper's
`NetPort` bind and connect rules (as a `CompatLevel::HardRequirement`) for the
proxied models, with the ephemeral range enumerated; the model-driven backend
selection, which fails closed without the required binary; the daemon's CONNECT
proxy (`agentd::proxy`) with a per-session credential, an allowlist, a bounded
handshake, and a byte tunnel; and the `--session-egress host:port` /
`--session-loopback` flags with the `NO_PROXY` injection.

Verified end to end:

- The Landlock filter permits a connect to the granted port and denies another,
  and denies all TCP when no network is granted (`linux.rs`).
- The proxy tunnels bytes to an allowed destination, refuses a denied one, and
  refuses a missing or wrong credential (`proxy.rs`).
- A real agent: `agentd/examples/acp_handshake.rs` drives `opencode2 acp`
  confined to `session.acp.ready` and through a prompt turn, both with
  `--sandbox` (private namespace, no egress) and with
  `--sandbox --egress openrouter.ai:443` (shared network, proxy port and
  ephemeral range open), where the proxy observes the agent's
  `CONNECT openrouter.ai:443`.

Not built: a private namespace with a veth to the host proxy (which would
remove the ephemeral-range gap without enumerating ports, but unprivileged
bubblewrap cannot create a veth), TLS-terminating credential injection, and a
per-provider base-URL rewrite. The default policy still denies egress entirely;
a session opts in with `--session-egress`.