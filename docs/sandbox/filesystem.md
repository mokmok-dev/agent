---
type: Design
title: filesystem
description: The sandbox path policy and its bubblewrap, Landlock, and Seatbelt rendering
tags:
  - sandbox
  - bubblewrap
  - landlock
  - seatbelt
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Filesystem Confinement

The filesystem boundary is the kernel's. A `Policy` is rendered into the
platform's native mechanism (bubblewrap or Landlock on Linux, Seatbelt on
macOS) and a command runs inside it. In-process path checks are defense in depth
for a future interpreter, never the guarantee: a spawned interpreter bypasses
them. See [security.md](./security.md) for the trust model.

## Policy Model

`Policy` has four domains and its zero value is fully inert: no path is
writable, no connection opens, and reads are the broad host grant that a `deny`
entry narrows.

| Domain    | What it controls                                                     |
| --------- | -------------------------------------------------------------------- |
| `fs`      | Path entries (`read` / `write` / `deny`) and protected metadata names |
| `shell`   | Environment and working directory                                    |
| `network` | Unix sockets and the egress proxy the command may reach              |
| `limits`  | Wall-clock timeout and output cap                                    |

```rust
pub struct Policy {
    pub fs: FsPolicy,
    pub shell: ShellPolicy,
    pub network: NetworkPolicy,
    pub limits: Limits,
}

pub struct FsPolicy {
    /// Path entries, evaluated with `deny > write > read`.
    pub entries: Vec<FsEntry>,
    /// Names fixed read-only inside any write root. Defaults to `.git` and
    /// `.agents`, so a command cannot rewrite its own instructions or the
    /// history it is diffed against.
    pub protected: Vec<String>,
}

pub struct FsEntry {
    pub path: PathBuf,   // a directory (subpath) or a file (literal)
    pub access: Access,  // Read | Write | Deny
}

pub struct ShellPolicy {
    pub env: Vec<EnvVar>, // the host environ is never inherited
    pub workdir: PathBuf, // must be covered by a `write` entry
}

pub struct Limits {
    pub timeout: Duration,
    pub max_output_bytes: u64,
}
```

`Limits` has a non-zero `Default` (a wall-clock timeout and an output cap), not
the `Duration::ZERO` / `0` a derived zero would give: a zero timeout would kill
every command the instant it started. `Policy::default()` therefore denies the
network and writes while still allowing a command to run and finish; the zeroed
`Limits` is a trap callers do not fall into.

`network` is specified in [network.md](./network.md); this document covers
`fs`, `shell`, and `limits`.

### Path entries

- **Precedence is `deny > write > read`.** An entry grants or removes access;
  the most restrictive matching entry wins, so a `deny` inside a broad `write`
  root holds.
- **Entries are validated at construction.** An entry must be an absolute host
  path that resolves. A `deny` must not cover the working directory, an
  executable directory, a write root, or the sandbox scratch directory — every
  command needs those — and a violation is an `InvalidPolicy` error, not a
  silently-ignored entry.
- **Paths are canonicalised.** The profile matches resolved paths, so `/tmp` is
  `/private/tmp` on macOS; entries and protected names are resolved before
  comparison. `~` is a shell expansion and is **not** performed on a path.
- **The working directory must be inside a `write` entry**, so a workspace a
  command cannot write is rejected rather than silently run read-only.
- **Symlinks are rejected at construction.** An entry that resolves through a
  symlink escaping its write root is an error, not a runtime surprise.

### Protected metadata

Inside a write root, each `protected` name is forced read-only, so the
protection holds even before the directory exists (a fresh `.git` cannot be
created). The default protects `.git` and `.agents`. The root path and the name
are escaped before they enter a rule, so a path arriving as event data cannot
inject a clause.

### Writable roots and renames

A writable root also refuses to be renamed or unlinked. Without that, a command
could replace the directory the *next* policy will treat as a boundary.

### Reads are broad, then narrowed

Reads are granted broadly (the whole host root), with `deny` entries removing
specific paths. A filtered read grant makes platform binaries abort before the
shell starts on macOS (a `SIGABRT` in `dyld4::CacheFinder`). Seatbelt evaluates
a deny ahead of a matching allow, so a broad read plus a narrow deny holds for a
spawned host binary. On Linux the same shape is the read-only host root with
`deny` masks over it.

## Linux Rendering

### bubblewrap (preferred)

bubblewrap gives a read-only host root, a private `/dev`, namespaces, and
network isolation in one mechanism. The command runs as:

```
bwrap \
  --ro-bind / /                       # broad read grant
  --dev /dev                          # private /dev
  --bind <write-root> <write-root>    # one per `write` entry
  --ro-bind <protected> <protected>   # re-bind protected names read-only
  --tmpfs <deny-path>                 # mask a `deny` after the binds
  --tmpfs <scratch>                   # the command's TMPDIR
  --dir <parent> --bind <socket> <socket>   # one per granted socket
  --unshare-all                       # pid, net, ipc, uts, cgroup, user
  --die-with-parent                   # no orphan outliving the daemon
  -- <command>
```

- **`deny` is applied after the binds as a mask.** A mount over an earlier grant
  overrides it, so a `deny` nested in a write root is hidden. This is coarser
  than macOS, which carves the path out read-only; it is a stated gap.
- **A granted socket is bound read-write, after the masks.** A Unix socket is a
  filesystem object that crosses the network namespace, so it is mounted into the
  command's view. It is the one grant rendered after the masks, because its
  parent may live under the scratch tmpfs and the tmpfs must exist first. It is
  bound read-write, which preserves the socket's host mode; a read-only bind also
  permits `connect`, so the read-write form is not a kernel requirement. Every
  socket the policy grants takes this path: each `network.unix_sockets` entry and
  the egress proxy's socket. A `deny` over a granted socket or its parent is a
  `RenderError::SocketDenied`, because a bind emitted after a mask would silently
  override the `deny`; a socket that does not exist is
  `RenderError::MissingSocket`, refused rather than skipped.
- **`--unshare-all` drops the network namespace.** A command has loopback and
  nothing else, and loopback is its own. The one route out is a UDS bind-mounted
  in; see [network.md](./network.md).
- **`/tmp` is not masked.** A `tmpfs` over `/tmp` would hide a granted read
  underneath it and would hide the daemon socket, which may default to
  `$TMPDIR/agent/daemon.sock`. `/tmp` is the host directory like any other:
  readable through the broad grant unless a `deny` names it, writable only
  through a `write` entry or the scratch directory a command gets as its
  `TMPDIR`.
- **The resolved shell is granted.** A NixOS-style host keeps interpreters in a
  store, so the executor resolves its shell from `PATH` (then `/bin/bash`, then
  `/bin/sh`) and grants the shell's directory. A resolved shell inside a policy
  write root is rejected: a repository cannot supply the very binary that runs.

### Landlock fallback

When bubblewrap is absent, or present but unable to build a namespace, a helper
binary applies a **Landlock allowlist** and a **seccomp deny-list** before
`exec`. See [The helper](#the-helper).

- Landlock **can only grant**, so the policy is rendered as a path allowlist:
  system roots read-execute, `read` entries read-only, `write` roots read-write.
- A `deny` the allowlist cannot express — one nested inside an allowed tree —
  makes construction **fail closed** rather than leave the path silently
  unprotected. Denials are subtractive and Landlock has no subtraction, so the
  operator must use bubblewrap or restructure the policy.
- The helper denies TCP connect and bind with Landlock network rules. Those
  need **ABI v4 (Linux 6.7)**; on an older kernel the helper is asked for the
  access as a hard requirement, so it fails before the command runs rather than
  run unfiltered.
- `AF_UNIX` is unaffected by network rules. It is a filesystem object and is
  governed by the path allowlist like any other file, so a granted socket is
  reachable and a denied one is not — but it is not path-scoped by address the
  way Seatbelt can scope a `remote unix-socket`.

### The helper

Applying confinement to a child without the `unsafe` `pre_exec` this workspace
forbids needs a separate program. The helper is a dedicated binary
(`sandbox-helper`) rather than an `argv[0]` overload of the daemon: a separate
binary is directly testable and needs no daemon wiring.

- Invocation: `sandbox-helper <spec> <program> [args...]`.
- It is found on `PATH`, or via `AGENTD_SANDBOX_HELPER`, and a helper inside a
  policy `write` root is rejected.
- It **fails closed**: any setup error exits non-zero before `exec`, so the
  command never runs unconfined.
- It is a workspace binary and must be built and installed next to the daemon or
  on `PATH`; a dependency build does not produce it.

## macOS Rendering (Seatbelt)

Seatbelt receives a profile that is `(deny default)` with explicit grants:

- Reads at `/`, then `deny` entries emitted as denials after the broad grant.
- `write` entries as file-write rules, with protected names carved out with a
  `(deny file-write* (regex #"^<root>/<name>(/.*)?$"))` rule.
- A writable root receives a root-unlink denial so the root itself cannot be
  replaced.
- Network is denied by `deny default`; the only grants are the policy's Unix
  sockets and the egress transport, specified in [network.md](./network.md).

Seatbelt is unsupported by Apple and its behavior can shift between OS releases.
Profiles are best-effort; see [security.md](./security.md).

## Backend Selection

Selection is by **capability, not presence**:

1. **bubblewrap** is preferred. The binary is looked up on `PATH`; a `bwrap`
   inside a policy write root is rejected. It is only chosen when it can
   actually build a namespace on this host: the executor **probes** it once by
   running a throwaway `--unshare-all`. A host that installs bubblewrap but
   forbids unprivileged user namespaces (a nested container, a daemon under
   `no_new_privs`) would otherwise fail at spawn after a session was announced.
2. **Landlock helper** is the fallback when bubblewrap is absent or cannot
   build a namespace.
3. **Neither** means the daemon refuses to spawn: the command does not run
   unconfined.

On macOS, Seatbelt is the only filesystem backend. It has no network namespace,
so egress there uses the weaker loopback-TCP transport; see
[network.md](./network.md).

## Testing Strategy

- **Precedence**: `deny` inside `write` holds; protected metadata cannot be
  created or modified; canonicalisation collapses `/tmp`.
- **Validation**: a `deny` over the workdir, an executable directory, a write
  root, or the scratch directory is an `InvalidPolicy`.
- **Escape**: `../` traversal, a symlink out of a root, writable-root rename, an
  environment leak, and a `deny` withholding a secret from a real spawned
  process.
- **bubblewrap**: the argument renderer is unit-tested; where a namespace can be
  built, a real write reaches a write entry, a denial masks a path, an outside
  write fails, the environment is scrubbed, and a timeout kills the process
  group. A host whose bubblewrap cannot build a namespace skips the spawn tests,
  because the probe would not select it there either.
- **Landlock**: the spec renderer, and a real Landlock process that confines the
  filesystem, refuses a denied read, denies TCP, and reports seccomp active
  through `/proc/self/status`; a nested `deny` rejects the fallback.
- **Seatbelt**: the profile renderer, and a spawned host binary that reads a
  granted path and is denied a denied one.
- **Differential**: golden files recorded from real bash and coreutils for the
  layer-1 behavior, replayed in CI without the recorded host.

## Stated Gaps

- **No hard memory ceiling.** macOS has no enforcement mechanism and Linux is
  not given a cgroup, so a limit field would be a false promise. The wall-clock
  timeout and output cap are enforced.
- **Reads are unconfined unless a `deny` names them.** With no denial a spawned
  host binary can read any file the user can. With egress denied the
  exfiltration channel is closed, but a secret read still reaches the model
  context, so operators should name credentials in `deny` entries.
- **A pre-existing hard link inside a write root aliases a file outside it.**
  The write allowlist matches paths, so such a link can be written through.
  Creating the link requires access outside the sandbox, so it is a
  precondition, not something a confined command can set up.
- **Linux denials are coarser than macOS.** With bubblewrap a nested `deny` is
  masked (hidden) rather than carved out read-only, and a fresh protected name
  can still be created inside a write root. With Landlock a nested `deny` cannot
  be expressed at all, so construction fails closed.
