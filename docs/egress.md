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
proxy on loopback TCP; the proxy opens the real connection only to an allowed
`host:port` and tunnels bytes with HTTP `CONNECT`.

```
+---------------- sandbox (OS-confined) ----------------+
|  ACP agent                                            |
|    |  loopback TCP bind (its own HTTP server)         |
|    |  loopback TCP connect -> 127.0.0.1:PROXY_PORT    |
+----|--------------------------------------------------+
     |                    OS grants: loopback bind + the proxy port only
     v
+---------------- daemon (trusted) ----------------------+
|  CONNECT proxy  -- allowlist(host:port) --> provider  |
+-------------------------------------------------------+
```

What each layer decides:

| Layer | Decides | Enforced by |
| --- | --- | --- |
| OS profile | loopback bind, the proxy port, everything else denied | Seatbelt / Landlock |
| Proxy | which `host:port` the tunnel may open | the daemon |
| Provider | authentication | the agent (the tunnel does not see it) |

The tunnel is **opaque**: the proxy does not terminate TLS and never sees the
provider credentials, which the agent holds and sends end to end. That is the
simpler and safer choice; a credential-injecting proxy would need a MITM CA the
agent must trust, and is a separate layer.

## Policy model

`NetworkPolicy` gains two grants beside its Unix sockets:

```rust
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to (unchanged).
    pub unix_sockets: Vec<PathBuf>,
    /// Loopback TCP ports the command may bind (its own local server).
    pub loopback_bind: Vec<u16>,
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

The zero value still grants nothing. `loopback_bind` and `proxy` are the only
IP grants; there is still no general host/IP allowlist in the OS profile.

## Platform rendering

| Concern | macOS Seatbelt | Linux (Landlock helper) | Linux (bubblewrap) |
| --- | --- | --- | --- |
| loopback bind | `(allow network-bind (local ip "localhost:P"))` per port | `NetPort::new(P, BindTcp)` per port | network grant forces the helper |
| proxy connect | `(allow network-outbound (remote ip "localhost:P"))` | `NetPort::new(P, ConnectTcp)` | network grant forces the helper |
| unix sockets | path-scoped `remote unix-socket` (existing) | unaffected (filesystem objects) | unaffected |

Notes and gaps:

- **Landlock is port-only.** A connect grant for port `P` allows `P` on *any*
  address, not just loopback, because Landlock has no host dimension. In
  practice `P` is the proxy's port bound on `127.0.0.1`, so reaching it on a
  remote address is not an egress channel; it is a stated imprecision, not a
  grant.
- **bubblewrap cannot express a port filter, so a network grant forces the
  Landlock helper.** Keeping `--unshare-net` would make the grant unreachable;
  dropping it with no filter would be a blanket reopen. The implementation
  therefore selects the helper whenever the policy grants network, even when
  `bwrap` is present, and **fails closed** if the helper is missing
  (`SandboxError::InvalidPolicy` explaining why). Composing `--share-net` with
  the helper is the coherent version and is not built yet.
- **DNS.** A hostname in the allowlist is resolved by the proxy, on the trusted
  side, so the child never needs the resolver. A child that resolves names
  itself still cannot: it has no route except the proxy.
- **TLS stays end to end.** The proxy sees `host:port` and ciphertext, never the
  provider key or the plaintext request.
- **NixOS-style hosts.** The executor's default shell and system roots assume
  FHS (`/bin/bash`, `/usr`, ...), so on a host where the interpreter lives in a
  store (`/nix/store`) the confinement probe fails before any network filtering
  is reached. That is a general portability gap, not specific to egress.

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

**Stated gap: the agent must honour the proxy.** The daemon injects
`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` into the sandbox environment, but whether
an agent's HTTP client uses them is the agent's choice. An agent that ignores
the proxy env simply cannot reach the network, because the OS grants it nothing
else — the failure is closed, not open.

## Testing strategy

- **Policy tests**: the zero value grants nothing; `loopback_bind` and `proxy`
  round-trip through JSON; a proxy with an empty allowlist is inert.
- **Rendering tests**: Seatbelt emits the loopback bind and proxy connect
  clauses; Landlock emits the `NetPort` bind/connect rules; a network grant with
  bwrap fails closed without the helper.
- **Proxy tests**: an allowed `host:port` tunnels bytes to a local listener; a
  denied host is refused with `403` and no connection is opened; a malformed
  request is rejected.
- **End-to-end**: a confined child reaches a local echo server only through the
  proxy, and a direct connection to the echo server's port is denied.

## Implementation status

Implemented: the `loopback_bind`/`proxy`/`HostPort` policy fields with
validation; the Seatbelt loopback bind and proxy connect clauses; the Landlock
helper's `NetPort` bind/connect rules; the fail-closed selection that forces the
helper when a network grant is present; the daemon's CONNECT proxy
(`agentd::proxy`) with a per-session credential, an allowlist, and a byte
tunnel; and the `--session-egress host:port` / `--session-loopback-bind PORT`
flags, which start the proxy and point the session policy and proxy env at it.

Verified end to end: the Landlock filter permits a connect to the granted port
and denies another (`linux.rs`); the proxy tunnels bytes to an allowed
destination, refuses a denied one, and refuses a missing or wrong credential
(`proxy.rs`).

Not built: composing `--share-net` with the helper for hosts that have
bubblewrap, TLS-terminating credential injection, a per-provider base-URL
rewrite, and the NixOS portability fix above. The default policy still denies
egress entirely; a session opts in with `--session-egress`.