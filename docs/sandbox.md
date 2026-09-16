---
type: Design
title: sandbox
description: AIエージェントがbashコマンドをsandbox上で安全に実行するための基盤設計。OSネイティブの隔離を唯一の境界とし、pathポリシーとネットワーク全面denyをPolicyとして提供し、許可判断と違反をCloudEventsとしてevent logに永続化する。
tags:
  - sandbox
  - seatbelt
  - bubblewrap
  - landlock
  - permission
  - eventlog
generated:
  by: human
  at: 2026-09-13T00:00:00Z
---

# agentd sandbox design

The sandbox is the confinement layer through which an agent drives shell
commands. **The boundary is the OS, not an in-process check**: the policy is
rendered into the platform's native isolation mechanism (Seatbelt on macOS,
bubblewrap/Landlock + seccomp on Linux) and a spawned command runs inside it.
Permission decisions and policy violations are durably appended as CloudEvents
to the event log, so approval flows and audit trails are built from the same
choreography model as everything else in `agentd`.

The design borrows its model from [openai/codex](https://github.com/openai/codex)
(see `docs/research/codex-sandboxing.md`) — argv rewriting plus kernel-enforced
confinement, deny-by-default, a filesystem path policy with `deny > write >
read`, and a network that is denied rather than left open. It also keeps the
one idea codex lacks: **every decision and every denial is a durable event**, not
a log line.

## Goals and non-goals

Goals:

- An agent can run bash commands with filesystem access confined by a `Policy`,
  enforced by the kernel.
- **No network egress.** The sandbox cannot reach any host on the network. The
  only reachable endpoint is the daemon's Unix socket, so the agent reaches its
  model provider *through the daemon*: the daemon holds the provider credentials
  and performs inference on the agent's behalf. The sandbox never holds network
  credentials and never opens an IP connection.
- Filesystem permissions are configurable per sandbox instance.
- Permission grants, denials, and policy violations are durable events in the
  log, enabling human-in-the-loop approval by any WS client.
- The execution strategy behind the shell is swappable.

Non-goals (stated honestly, per the Sheena precedent):

- No hard per-process memory ceiling on macOS (no cgroups equivalent); memory
  caps are best-effort there.
- The sandbox is not a boundary for hostile native code. It confines the tool
  calls an agent *requests* against a configured policy.
- **No general network allowlist.** Egress is denied outright, not filtered by
  host. A future non-daemon remote service would need the managed-proxy model
  (codex's `network-proxy` + netns bridge) as a separate, opt-in layer; it is
  not built now because the daemon-mediated path covers inference.
- **Cedar is not adopted.** A policy language was considered for the path rules
  and dropped: the rules are path lists with three access levels, and the layer-1
  profile is rendered from them directly. Revisit only if policies must be
  authored outside the Rust code.

## Design principles

1. **Deny-by-default.** Nothing runs, no file is written, no connection is
   opened until the policy opts in. The zero value of a `Policy` is fully inert.
2. **The OS is the only boundary.** Path confinement that the kernel itself
   guarantees (Seatbelt profiles, bubblewrap bind mounts, Landlock rulesets) is
   the boundary. In-process path checks are defense in depth for future
   executors, never the guarantee — a confused-deputy command can bypass them.
3. **One path policy, three access levels.** A single list of path entries
   (`read`, `write`, `deny`) with the precedence `deny > write > read` is the
   whole filesystem policy. There is no separate "mount", "refuse", and
   "hide" machinery to reconcile.
4. **Swappable executors.** The shell execution strategy sits behind an
   `Executor` trait. Layer 1 maps the policy onto the OS. A future layer 2 may
   add an in-process interpreter without changing the policy or event contract.
5. **Every decision is an event.** Each permission check produces a CloudEvent
   (`requested` → `granted`/`denied`) and each policy violation produces a
   `sandbox.violation.*` event, durably appended to the log, so the audit log is
   the log itself.
6. **Kernel-enforced over convention-enforced.** Where the OS offers a stronger
   primitive (bubblewrap mount namespaces over Landlock path rules, a writable
   root's unlink denial over trusting a path check), use it.

## Policy model

`Policy` has four domains; its default is deny-everything:

| Domain    | What it controls                                                                 |
| --------- | -------------------------------------------------------------------------------- |
| `fs`      | Path entries (`read`/`write`/`deny`), protected metadata, byte caps               |
| `shell`   | Allowed command prefixes (a UX guard, not a boundary), environment, working directory |
| `network` | Nothing configurable; **egress and ingress are denied**                           |
| `limits`  | Wall-clock timeout, command count, output bytes, best-effort memory cap          |

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
    /// Names fixed read-only inside any writable root. Defaults to `.git` and
    /// `.agents`, so a command cannot rewrite its own instructions or the repo
    /// history it is diffed against.
    pub protected: Vec<String>,
    /// Host paths whose read access the OS profile withholds (absolute).
    pub deny_read: Vec<PathBuf>,
    pub max_total_bytes: Option<u64>,
    pub max_file_bytes: Option<u64>,
}

pub struct FsEntry {
    pub path: PathBuf,   // a directory (subpath) or a file (literal)
    pub access: Access,  // Read | Write | Deny
}

pub struct ShellPolicy {
    pub allow: Vec<CommandPrefix>, // deny-by-default; empty means no command runs
    pub env: EnvAllowlist,         // the host environ is never inherited
    pub workdir: PathBuf,
}

pub struct Limits {
    pub timeout: Duration,
    pub max_command_count: u32,   // fork-bomb / runaway loop guard
    pub max_output_bytes: u64,
    pub max_memory_bytes: Option<u64>,
}
```

### Filesystem entries

- **Precedence is `deny > write > read`.** An entry grants or removes access;
  the most restrictive matching entry wins, so a `deny` inside a broad `write`
  root holds.
- **Read is granted broadly, then narrowed.** On macOS 26 a filtered read grant
  makes platform binaries abort inside `dyld4::CacheFinder` (`SIGABRT` before the
  shell starts), so reads are granted at `/` and the `deny_read` entries are
  emitted as denials after the broad grant. Seatbelt evaluates a deny ahead of a
  matching allow, so this holds for a spawned host binary.
- **Entries are validated.** An entry must be an absolute host path that
  resolves. A `deny` must not cover the workdir, an executable directory, or the
  sandbox scratch directory — every command needs those — and a failure there is
  an `InvalidPolicy` error at construction, not a silently-ignored entry.
  `~` is a shell expansion and is *not* performed on a path.
- **Canonicalisation is required.** The profile matches resolved paths, so
  `/tmp` is `/private/tmp`; both the entries and the protected paths are resolved
  before comparison.

### Protected metadata

Inside a writable root, `protected` names are forced read-only with a profile
rule of the shape `^<root>/<name>(/.*)?$`, so the protection holds even before
the directory exists (a fresh `.git` cannot be created). The default protects
`.git` and `.agents`. This is a *carveout* on top of the write grant, expressed
as both `require-not (literal ...)` and `require-not (subpath ...)` so the
protected directory itself and everything under it are withheld.

### Writable roots and renames

A writable root receives a `(deny file-write-unlink (require-all (literal
<root>) (vnode-type DIRECTORY)))` rule, so a command cannot rename or unlink the
root itself. Without it, a command could replace the directory the *next*
sandbox profile will treat as a boundary. User-controlled symlinks inside a
writable root are rejected at construction (`SeatbeltPreparationError`); the
top-level macOS alias `/tmp` → `/private/tmp` is the only symlink allowed.

### Network

Network has no allow surface. Egress and ingress are denied at the OS level, with
exactly one exception: the daemon's Unix domain socket path, so a sandboxed node
or agent can speak to the daemon. `AF_UNIX` is the only socket family a confined
command may use; `AF_INET`/`AF_INET6` are denied, and on Linux a seccomp filter
enforces it in addition to the network namespace.

Inference — the reason an earlier design left egress open — is instead a daemon
capability: the agent asks the daemon over the socket, and the daemon holds the
provider credentials and reaches the network. The sandbox thus has no
exfiltration channel, so per-host rules and an SSRF guard are unnecessary while
the only reachable endpoint is a local socket. A future remote service that must
be reached directly would reintroduce the managed-proxy model as a separate
opt-in.

### Shell allowlist

`allow` is deny-by-default, but it is a **convenience guard, not a security
boundary**: layer 1 spawns real host binaries and the OS profile, not the prefix
list, decides what they can reach. A prefix is a token-wise prefix of a command
clause; an empty or whitespace-only prefix is rejected at construction, because
an empty token list would otherwise match every command.

## Architecture

The sandbox ships as a first-class workspace crate, `agentd-sandbox`: it depends
on `agentd-events` only and is wired into `agentd` behind a cargo feature
(`sandbox = ["dep:agentd-sandbox"]`).

```mermaid
flowchart LR
    A["Agent<br/>(log subscriber)"] -- "command request" --> E["Executor trait"]
    P["Policy"] -- "bound at construction" --> E
    E -- "renders a path policy" --> K["OS confinement<br/>Seatbelt, bwrap, Landlock, seccomp"]
    K -- "confined spawn" --> B["bash process"]
    B -- "only the daemon UDS" --> N["daemon"]
    E -- "permission + violation events" --> L["EventLog"]

    style B fill:#f8f8f2,stroke:#888
```

| Component                  | Crate            | Role                                                                                     |
| -------------------------- | ---------------- | ---------------------------------------------------------------------------------------- |
| `Policy`                   | `agentd-sandbox` | The four-domain deny-by-default configuration; serializable so it can arrive as an event. |
| Path renderer              | `agentd-sandbox` | Renders `FsPolicy` entries into a Seatbelt profile / bwrap args / Landlock ruleset.       |
| `Executor` trait           | `agentd-sandbox` | Runs a command string under a policy; returns `Result { stdout, stderr, exit_code }`.     |
| `ConfinedProcessExecutor`  | `agentd-sandbox` | Layer 1: builds the OS profile and spawns a real bash.                                    |
| Violation classifier       | `agentd-sandbox` | Maps an exit code and output to a structured `sandbox.violation.*` event.                 |
| Permission publisher       | `agentd-sandbox` | Emits `sandbox.permission.*` CloudEvents for every policy evaluation.                     |

### Executor trait

```rust
pub trait Executor: Send + Sync {
    /// The policy is bound at executor construction, not per call, so a
    /// running executor cannot widen its own permissions.
    async fn exec(&self, command: &str) -> ExecResult;
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub denied_by: Option<DenialReason>, // set when the policy refused the command
}
```

### Layer 1 backends

| Concern                | Linux                                                       | macOS                                       |
| ---------------------- | ----------------------------------------------------------- | ------------------------------------------- |
| Filesystem confinement | bubblewrap bind mounts (preferred); Landlock ABI V5 fallback | Seatbelt profile `(deny default)`            |
| Syscall narrowing      | seccomp filter (block `ptrace`, `io_uring_*`, network)       | not available; profile covers most          |
| Process isolation      | `--unshare-user/pid/ipc`, `--unshare-net`                    | not available                               |
| Network                | `--unshare-net` + seccomp; UDS allowed by bind mount         | outbound denied; the daemon UDS path allowed |
| Memory / rlimits       | `prlimit` + optional cgroups v2                              | `setrlimit` (best-effort)                   |
| Host binary control    | confined `PATH` built from the shell allowlist               | same                                        |

**bubblewrap is preferred over Landlock** because it gives read-only root binds,
a private `/dev`, namespaces, and `--cap-drop ALL` in one mechanism, whereas
Landlock confines paths only. `find_system_bwrap_in_path` must exclude a `bwrap`
inside the workspace, so a repo cannot supply the very binary that builds the
boundary. When the system bwrap lacks `--ro-bind-fd`, it is rewritten to
`--ro-bind /proc/self/fd/<fd>` with a mount verification.

**One binary, two personalities.** The Linux helper is not a second binary:
`agentd` places a `agentd-sandbox` symlink to its own executable in its runtime
directory, prepends that directory to `PATH`, and dispatches on `argv[0]`
(`codex-linux-sandbox`'s arg0 trick). This keeps the helper on a trusted path and
avoids shipping and locating a separate executable.

## Layer 2 (planned): the in-process VFS

The `Vfs` trait (`Mem`, `ReadOnlyMount`, `ReadWriteMount`, `Overlay`) is **not**
part of layer 1's boundary and does **not** constrain spawned commands. It exists
for a future in-process interpreter executor that re-implements commands and
therefore has no kernel boundary. Only there do the `Overlay` copy-on-write and
glob-based `refuse`/`hide` semantics apply. Keeping the trait is worthwhile
because it is the only confinement available to such an executor; treating it as
the layer-1 boundary was the earlier design's error.

## Permission and violation events

Every policy evaluation and every OS-enforced denial publishes events with the
existing dotted-type convention and `source: urn:mokmokd`:

| Kind                          | When                                                              |
| ----------------------------- | ----------------------------------------------------------------- |
| `sandbox.permission.requested` | A command or resource access needs a decision                     |
| `sandbox.permission.granted`  | A static policy rule or an approver allowed it                    |
| `sandbox.permission.denied`   | A static policy rule or an approver refused it (deny wins)        |
| `sandbox.violation.filesystem` | The OS refused a filesystem operation: exit code, reason, path, snippet |
| `sandbox.violation.network`   | The OS refused a network operation                                 |
| `sandbox.exec.completed`      | Terminal state of an execution: exit code, duration, output sizes |

`decision` in `requested` is `pending` when human approval is configured and
`auto` when a static rule decided immediately. An approver is a WS client that
reacts to `requested` by publishing a `granted`/`denied` event carrying the same
`sandbox_id`; it authenticates with a token holding the `authority` claim, so a
client without it cannot publish a `sandbox.permission.*` event (see
[architecture](architecture.md#access-control)).

A violation event is the improvement over codex, which classifies
`operation not permitted` / `read-only file system` / `SIGSYS` but only emits a
`tracing::warn`: here the reason (`OperationNotPermitted`, `ReadOnlyFileSystem`,
`PolicyDenied`, `SignalSyscall`, ...), the backend, and a bounded output snippet
become durable, correlated state.

## Security guarantees and stated gaps

Prevented:

- Path traversal and symlink escape out of a writable root, and replacement of
  the writable root itself (kernel-enforced on Linux via mount namespaces; the
  Seatbelt profile on macOS, with symlink rejection at construction).
- Rewriting `.git` or `.agents` inside a writable root (protected-carveout
  rules).
- Host environment leakage (env is an allowlist; the host environ is never
  inherited).
- Fork bombs and runaway loops (command count + timeout).
- Silent host contamination by default (writes require an explicit `write`
  entry).
- **Network egress.** A confined command cannot open an IP connection; the
  daemon's Unix socket is the only reachable endpoint.
- Reads of the paths named in `FsPolicy::deny_read`, held against a spawned host
  binary and verified end to end.

Stated gaps:

- **No hard memory ceiling for spawned commands on macOS.** `setrlimit` is
  best-effort; a truly hard cap requires cgroups, which are Linux-only.
- **Layer 1 runs real host binaries.** A command inside the allowlist prefix can
  still do surprising-but-confined things. Prefix allowlisting is convenience,
  not proof of intent — the confinement boundary is the OS profile.
- **macOS Seatbelt is officially unsupported by Apple.** It is functional and
  widely used, but profiles are best-effort and behavior can shift between OS
  releases.
- **Reads are unconfined unless `deny_read` names them.** With an empty
  `deny_read`, a spawned host binary can read any file the user can. With egress
  denied outright the exfiltration channel is closed, but a secret read still
  reaches the model context, so operators should name credentials in
  `deny_read`.
- **A hard link inside a writable root aliases a file outside it.** The write
  allow-list matches paths, so a pre-existing hard link under the root can be
  written through. Creating the link requires access outside the sandbox, so it
  is a precondition, not something a confined command can set up.
- **Linux is not implemented yet.** The bwrap/Landlock/seccomp backends and the
  arg0 helper are the immediate follow-up; the crate compiles on Linux but runs
  nothing.

## Testing strategy

Modeled on Sheena's methodology and codex's, adapted to Rust:

- **Category-organized security tests**: sandbox escape (`../`, symlink out of a
  root, `/proc`, `/etc`), writable-root rename, limits (DoS, output flood,
  command count, timeout), **egress denied and the daemon UDS reachable**, env
  leakage, and `deny_read` withholding a secret from a real spawned process.
- **Path precedence tests**: `deny` inside `write` holds; protected metadata
  cannot be created or modified; canonicalisation collapses `/tmp`.
- **Permission and violation flow tests**: end-to-end through the log —
  `requested` → `granted`/`denied` correlation, deny-wins semantics,
  pending-timeout behavior, and a structured `sandbox.violation.*` for a real
  OS denial.
- **Differential tests**: golden files recorded from real bash + coreutils for
  layer 1 behavior, replayed in CI without the recorded host.
- **Benchmarks**: sandbox construction, trivial `exec` overhead, parallel
  sandbox throughput — the "lighter than a container" claim must be measured.

## Roadmap

Triggers, not dates — none of these steps are taken early:

1. Render `FsPolicy` path entries into the macOS Seatbelt profile (writable
   roots, `deny_read`, protected metadata, root-unlink denial), replacing the
   `mounts`/`refuse`/`hide` shape.
2. Deny network egress at the OS level and allow only the daemon UDS.
3. Linux backend: bubblewrap preferred, Landlock fallback, seccomp for network
   and syscall narrowing, with the arg0 self-exec helper.
4. Human-in-the-loop approval flow over the WS event API, using the `authority`
   claim from `docs/architecture.md`.
5. A long-lived session spawn API: today `Sandbox::exec` is a one-shot bounded
   by a timeout that waits for the child to exit. A session needs a supervised
   process that outlives one command, with a writable session mount for its
   SQLite projection (`docs/node.md`).
6. A session manager in `agentd` that launches a sandboxed node and reports
   lifecycle through `session.*` events.
7. Optional layer 2: the in-process interpreter backend and its VFS.

## Implementation status

This document is the target. The code currently implements an earlier shape:
`FsPolicy` still carries `mounts`/`refuse`/`hide`/`deny_read`, `NetworkPolicy`
is empty with egress open in the rendered profile, the `Vfs` trait is not wired
to the executor, and the Linux backend is a fail-closed stub. The migration is
tracked by the roadmap above; the crate's code comments record the same
deviations next to the code, so a reader never has to reconcile two sources of
truth from memory.
