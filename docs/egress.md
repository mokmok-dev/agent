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
  proxy. This needs the *shared* network with one port open, which is the
  Landlock helper's `NetPort` filter.

They are **mutually exclusive**: a private namespace cannot reach the proxy on
the host's loopback, and a shared namespace cannot confine loopback to the
command. `Policy::validate` rejects a policy that asks for both.

## Policy model

```rust
pub struct NetworkPolicy {
    /// Unix domain sockets the command may connect to (unchanged).
    pub unix_sockets: Vec<PathBuf>,
    /// Free loopback: the command may bind its own server and connect to it on
    /// any port. Served by a private network namespace (no egress).
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
grants; `loopback` needs no port list because the namespace, not a port filter,
is the boundary.

## Platform rendering

| Concern | macOS Seatbelt | Linux |
| --- | --- | --- |
| loopback | `(allow network-bind (local ip "localhost:*"))` and `(allow network-outbound (remote ip "localhost:*"))` | bubblewrap `--unshare-all`: a private namespace where only loopback exists |
| proxy connect | `(allow network-outbound (remote ip "localhost:P"))` | the Landlock helper's `NetPort::new(P, ConnectTcp)`, shared namespace |
| unix sockets | path-scoped `remote unix-socket` (existing) | unaffected (filesystem objects) |

Backend selection follows from the model:

- `loopback` requires **bubblewrap** (the private namespace); without it,
  construction **fails closed**.
- `proxy` requires the **Landlock helper** (the port filter); without it,
  construction **fails closed**.
- neither prefers bubblewrap and falls back to Landlock for the filesystem and
  seccomp.

Notes and gaps:

- **Landlock is port-only, and that port is reachable on any address.** A
  connect grant for port `P` allows `P` on *any* address, not just the proxy on
  loopback, because Landlock has no host dimension. The child learns `P` from
  `HTTP_PROXY` and could connect to `P` on an arbitrary remote host. Per
  session that is one port, not a general channel, but it is a narrower
  arbitrary-egress path than "the proxy only" — a real limitation. This is the
  proxy model only; the `loopback` model has no such path, because its
  namespace has no remote host at all.
- **Landlock network rules need ABI v4 (Linux 6.7).** On an older kernel the
  crate would drop the handling silently and the `NetPort` rule would become a
  no-op, so a proxied session would run **unfiltered**. The helper requests the
  net access as a `CompatLevel::HardRequirement`, so a kernel that cannot
  enforce it fails `handle_access` before the command runs.
- **A loopback session verified against a real agent.** `opencode2 acp`
  (`--sandbox` in `agentd/examples/acp_handshake.rs`) completes its handshake
  under `bwrap --unshare-all` with loopback bind and connect working and no
  external route; the same agent fails under the Landlock fallback, which cannot
  express free loopback without opening egress.
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

**Stated gap: the agent must honour the proxy.** The daemon injects
`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` into the sandbox environment, but whether
an agent's HTTP client uses them is the agent's choice. An agent that ignores
the proxy env simply cannot reach the network, because the OS grants it nothing
else — the failure is closed, not open.

## Testing strategy

- **Policy tests**: the zero value grants nothing; `loopback` and `proxy`
  round-trip through JSON; a policy asking for both is rejected.
- **Rendering tests**: Seatbelt emits the loopback clauses and the proxy connect
  clause; Landlock emits the `NetPort` connect rule; a loopback grant with no
  bubblewrap fails closed.
- **Proxy tests**: an allowed `host:port` tunnels bytes to a local listener; a
  denied host is refused with `403`; a missing or wrong credential is refused
  with `407`; a client half-close still receives the reply.
- **End-to-end**: a confined child reaches a local echo server only through the
  proxy, and a direct connection to the echo server's port is denied; a real ACP
  agent completes its handshake in a private namespace with no egress.

## Implementation status

Implemented: the `loopback`/`proxy`/`HostPort` policy fields with validation
(including the mutual exclusion); the Seatbelt loopback and proxy clauses; the
bubblewrap private-namespace model for `loopback`; the Landlock helper's
`NetPort` connect rule (as a `CompatLevel::HardRequirement`) for `proxy`; the
model-driven backend selection, which fails closed without the required binary;
the daemon's CONNECT proxy (`agentd::proxy`) with a per-session credential, an
allowlist, a bounded handshake, and a byte tunnel; and the
`--session-egress host:port` / `--session-loopback` flags.

Verified end to end:

- The Landlock filter permits a connect to the granted port and denies another
  (`linux.rs`).
- The proxy tunnels bytes to an allowed destination, refuses a denied one, and
  refuses a missing or wrong credential (`proxy.rs`).
- A real agent: `agentd/examples/acp_handshake.rs --sandbox` drives `opencode2
  acp` to `session.acp.ready` under `bwrap --unshare-all` — loopback works, no
  external route exists — and the same run under the Landlock fallback cannot,
  which is why `loopback` requires bubblewrap.

Not built: composing `--share-net` with the helper for hosts that have
bubblewrap, TLS-terminating credential injection, a per-provider base-URL
rewrite, and the NixOS portability fix above. The default policy still denies
egress entirely; a session opts in with `--session-egress`.