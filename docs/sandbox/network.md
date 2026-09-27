---
type: Design
title: network
description: Network isolation via a Unix-domain-socket HTTP proxy, the child-side forwarder, and transport selection
tags:
  - sandbox
  - network
  - proxy
  - unix-domain-socket
  - egress
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Network Isolation

Egress is **denied by default**. A confined command has loopback and nothing
else; it cannot open an IP connection to any host. When a policy opts into
egress, the operating system grants exactly one path out — a Unix domain socket
to a CONNECT proxy the daemon runs — and the proxy decides which destinations
are reachable.

This document covers the transport. The decision logic that makes destinations
mutable at runtime is in [permissions.md](./permissions.md).

## Why the OS Cannot Hold a Host Allowlist

The intuitive design — teach `NetworkPolicy` a `host:port` egress allowlist — is
not expressible on either platform:

- **macOS Seatbelt** can render `(allow network-outbound (remote tcp "host:port"))`,
  but the profile is frozen at `exec`, so the allowlist cannot change while the
  command runs.
- **Linux bubblewrap** drops the network namespace with `--unshare-all`; it has
  no per-host filter at all.
- **Linux Landlock** network rules are **per port only** (`ConnectTcp` /
  `BindTcp`), with no host dimension. A `host:port` allowlist cannot be
  enforced.

So the kernel can decide *whether* a port is reachable, never *which hosts*,
and any kernel decision is static. That is the constraint the design starts
from.

## The Inversion: One Path Out, Decisions in the Proxy

Move the destination decision out of the kernel and into a userspace proxy, and
give the kernel a job it can do: make the proxy the **only** route.

```
+-------------------- sandboxed command ---------------------------------+
|  private network namespace: loopback only, no external route          |
|                                                                       |
|   agent / bash                                                        |
|     |  HTTP_PROXY = http://agent:<token>@127.0.0.1:FORWARD            |
|     v                                                                 |
|   forwarder  (dumb: loopback TCP -> mounted UDS)                      |
+---------|-------------------------------------------------------------+
          |  Unix socket (a filesystem object; crosses the netns)
          v
+-------------------- daemon (trusted) ---------------------------------+
|  CONNECT proxy                                                        |
|    allowlist(host:port)  <-- mutable while the command runs           |
|    unlisted -> ask an approver over the event bus                     |
|    |                                                                  |
|    v                                                                  |
|  upstream host:port   (TLS stays end to end; the proxy sees ciphertext)|
+-----------------------------------------------------------------------+
```

| Layer | Decides | Enforced by |
| --- | --- | --- |
| Network namespace | loopback only; no external route, TCP or UDP | bubblewrap `--unshare-all` |
| Filesystem profile | the command sees exactly one proxy socket | bind mount / path allowlist |
| **Proxy** | which `host:port` a tunnel may open, **and when** | the daemon (mutable) |
| Provider | authentication | the agent (the tunnel never sees it) |

The kernel grants a transport, not a host. Because the proxy is consulted per
connection and holds mutable state, the reachable set can change while the
sandbox runs — the property no kernel policy could provide.

### Why a Unix socket, not loopback TCP

A Unix socket is a filesystem object, so it **crosses a network namespace**: the
daemon binds it on the host and the command gets it bind-mounted in. That lets
the command run in a **private network namespace** with *no IP route at all* and
still reach the proxy. Loopback TCP cannot do this: inside a private namespace
the host's loopback is not the command's, so a loopback proxy would require
sharing the host network and a port-only filter — which reintroduces the
bypasses a namespace removes:

- no IP route exists, so the command cannot bypass the proxy to an arbitrary
  host;
- no external interface exists, so UDP/QUIC cannot be used to escape;
- the proxy's address is not a host-reachable port.

## Policy Model

```rust
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to, by path. Nothing by default.
    pub unix_sockets: Vec<PathBuf>,
    /// Free loopback: the command may bind its own server and connect to it on
    /// any ephemeral port. On Linux this lives inside the private namespace.
    pub loopback: bool,
    /// The daemon proxy the command may reach. `None` grants no egress.
    pub proxy: Option<ProxyGrant>,
}

pub struct ProxyGrant {
    /// The loopback port the command's `HTTP_PROXY` names (the forwarder on
    /// Linux, the proxy itself on macOS).
    pub port: u16,
    /// The proxy's Unix socket (Linux). When set, the command reaches it through
    /// the forwarder; when `None`, the proxy is loopback TCP (macOS).
    pub socket: Option<PathBuf>,
    /// The destination set the proxy starts with, authored by the operator. The
    /// proxy may later add or revoke entries at runtime; see permissions.md. The
    /// zero set denies everything.
    pub egress: Vec<HostPort>,
}

pub struct HostPort {
    pub host: String,
    pub port: u16,
}
```

The zero value still grants nothing.

## The Two Mechanisms

A command may need the network for one of two reasons:

- **Loopback only.** An agent may start its own HTTP server on an ephemeral port
  and connect to it. A **private network namespace** expresses "loopback, and
  nothing else": inside it `127.0.0.1` is the command's own and every other
  address is unreachable (`Network is unreachable`).
- **Remote egress.** A command reaches a provider through the daemon's CONNECT
  proxy, which enforces the allowlist.

A networked agent needs **both**. They compose on Linux because the Unix socket
crosses the namespace: the command still runs with no IP route, yet it reaches
the proxy through the mounted socket and reaches its own loopback server
directly.

## Environment Injection

When egress is granted, the daemon injects:

| Variable | Value |
| --- | --- |
| `HTTP_PROXY` | `http://agent:<token>@127.0.0.1:<port>` |
| `HTTPS_PROXY` | the same |
| `ALL_PROXY` | the same |
| `NO_PROXY` | `127.0.0.1,localhost,::1` |

- The four names are **replaced, not appended**. An operator-authored value is
  dropped first, so the effective value cannot depend on ordering.
- `NO_PROXY` keeps the agent's own loopback off the tunnel. Without it an agent
  routes its internal server calls through the proxy and its session setup
  fails.
- The URL carries a **per-proxy token**. The proxy answers `407` to a client
  that does not present it as `Proxy-Authorization`, so another process of the
  same user cannot reuse the tunnel. The token is per proxy and dies with it.

The `HTTP_PROXY` form is a deliberate constraint: it is how nearly every HTTP
client and agent runtime already discovers a proxy, and pointing it at loopback
is all a confined command needs. A command that ignores the proxy environment
cannot reach the network, because the kernel grants it nothing else — the
failure is **closed**, not open.

## The CONNECT Proxy

The proxy is a small HTTP `CONNECT` server on either a **Unix socket** (Linux)
or **loopback TCP** (macOS):

1. It reads the `CONNECT host:port` request line under one handshake timeout and
   one size cap, so a pre-authentication client cannot stall the task or grow
   memory without bound.
2. It authenticates the `Proxy-Authorization` credential before revealing
   anything about the allowlist.
3. It consults the destination set. If allowed, it opens the upstream TCP
   connection; if not, it asks an approver
   ([permissions.md](./permissions.md)); if refused, it answers `403`.
4. On a grant it answers `200 Connection Established` and tunnels bytes both
   ways, honouring half-close: when one side finishes writing it shuts its
   counterpart down and the other direction keeps flowing.

The tunnel is **opaque**: the proxy does not terminate TLS and never sees the
provider credentials, which the command holds and sends end to end. A
credential-injecting proxy would need a MITM CA the command must trust, and is a
separate layer.

DNS is resolved by the proxy, on the trusted side, so the command never needs a
resolver. A command that resolves names itself still cannot: it has no route
except the proxy.

## The Child-Side Forwarder

A command inside a private namespace has no IP route, so it cannot reach a proxy
on the host's loopback. The **forwarder** (`egress-forward`) listens on loopback
inside the namespace and pipes every connection to the mounted socket, so the
command's `HTTP_PROXY` can point at `127.0.0.1` as usual.

It is deliberately **dumb**: it knows exactly one destination (the socket) and
carries no allowlist or decision. The proxy on the other end of the socket is
the trust boundary. It never parses the payload beyond copying bytes, so the
decision cannot be bypassed by confusing the forwarder.

The forwarder is resolved as a sibling of the running daemon (installed next to
it), then on `PATH`, then via an override variable. Every candidate follows the
same trust rule as the helper: a binary inside a policy write root is rejected.

## Transport Selection

Transport is a property of the **host**, chosen in one place, not by the
caller:

| Host capability | Transport | Filesystem grant |
| --- | --- | --- |
| Can build a private network namespace | Unix socket + forwarder | the socket is bind-mounted in |
| No namespace (macOS Seatbelt) | loopback TCP | the profile grants the port |
| Linux without bubblewrap | **fails closed** | egress refused |

- The Unix-socket form makes the socket the only route, so the boundary holds.
- The loopback-TCP form is weaker: Landlock and Seatbelt grant a **port** with
  no host dimension, so the granted port is reachable on any address. It exists
  only for a host that cannot do better.
- On Linux the daemon **refuses egress outright** when bubblewrap is missing
  rather than fall back to the weaker form: Linux can express the strong form,
  so downgrading would widen the boundary on the one host that gets it right.

### Platform rendering

| Model | macOS Seatbelt | Linux |
| --- | --- | --- |
| loopback | `(allow network-bind/local ip "localhost:*")` + outbound | `--unshare-all`: a private namespace where only loopback exists |
| proxy (Unix socket) | — | private namespace + the mounted socket + `egress-forward` |
| proxy (loopback TCP) | `(allow network-outbound (remote ip "localhost:P"))` | Landlock helper `NetPort(P, ConnectTcp)` |
| unix sockets | path-scoped `remote unix-socket` | unaffected (filesystem objects) |

The macOS Unix-socket grant is path-scoped: each socket renders as
`(allow network-outbound (remote unix-socket (regex #"^.*<path>$")))`. Path-scoped
`literal`/`subpath` filters do **not** work for a Unix socket on macOS, and an
unfiltered `(allow network-outbound)` would open all IP egress, so the
`remote unix-socket` form is the one that keeps egress denied while permitting
exactly the daemon socket. The trailing `$` rejects a sibling socket like
`<path>X`.

## Testing Strategy

- **Policy**: the zero value grants nothing; `loopback` and `proxy` round-trip
  through JSON.
- **Rendering**: Seatbelt emits the loopback clauses and the proxy connect
  clause; Landlock emits the `NetPort` connect rules for a loopback-TCP proxy; a
  private-namespace grant with no bubblewrap fails closed; a Unix-socket proxy
  renders the socket mount and the forwarder.
- **Transport**: the transport follows the host's namespace capability, and
  Linux without one fails closed.
- **Forwarder**: it bridges loopback to a Unix socket, and parses its CLI.
- **Proxy**: an allowed destination tunnels bytes; a denied one is `403`; a
  missing or wrong credential is `407`; a client half-close still receives the
  reply; a Unix-socket proxy tunnels too; the socket and its private directory
  are removed on stop.
- **End to end**: inside one `--unshare-all` namespace an external address is
  `Network is unreachable` while the forwarder's loopback port carries bytes to
  the mounted socket; a confined command completes a `CONNECT` handshake and
  receives the origin's body.

## Stated Gaps

- **An opted-in session weakens the network boundary by design.** It is the one
  case where a confined command can reach a remote host. On Linux the tunnel is
  the only route, so the boundary holds; on macOS the loopback-TCP proxy is
  port-only and host-agnostic, and a same-user local process can also reach the
  command's loopback server.
- **The command must honour the proxy.** The daemon sets the proxy environment,
  but whether a client uses it is the client's choice. An ignoring client fails
  closed, not open.
- **`AF_UNIX` sockets are not path-scoped by address on Linux.** They are
  filesystem objects governed by the path allowlist, so a granted socket is
  reachable and a denied one is not, but the kernel does not additionally scope
  the connect by address.
- **Landlock network rules need ABI v4 (Linux 6.7).** On an older kernel the
  fallback would drop the rules silently, so it is requested as a hard
  requirement and fails before the command runs instead.
