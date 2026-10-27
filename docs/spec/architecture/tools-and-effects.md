---
type: Protocol Spec
title: Tools and effects
description: Every external effect is bracketed by an intent and a result, keyed for idempotency, and gated by an approval policy.
tags: [tools, effects, idempotency, security]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# The problem

Replay is safe for state and unsafe for effects. Folding a million events twice produces the same state. Running a shell command twice does not produce the same world.

The store records intent before the effect and result after it, so a crash leaves evidence of what might have happened.

# The pair

Every effect is bracketed by two events and sometimes a third.

| Event | Carries |
|---|---|
| `tool.intent` | Call id, tool name, arguments, idempotency key, what the caller expects |
| The effect | Nothing. It happens outside the store |
| `tool.result` | Call id, exit status, duration, references to captured output, whether the output was truncated |
| `tool.failed` | Call id, error class, and whether the effect may have happened |

`may_have_happened` is true when the process had started and the outcome is unknown, for example a kill during execution or a crash before the result was synced.

# Idempotency keys

The key is a hash of the tool name, the canonicalized arguments, and the call id.

The call id is derived from the turn id and the step index, so a re-fold of the same turn produces the same key. That is what makes the pair replay-stable.

On recovery, an intent without a result is handled by the tool's declared class. Whether a call is in recovery is a question only the running process can answer: a call it filed itself is a first run, and a call it did not is one a process that is gone left behind.

One case looks like recovery and is not. A call waiting for an answer when its process died has an intent and no result, but the effect cannot have happened: a call does not run before its permission is recorded. When the yes is written by the process that is now holding the call, it is the first yes that call ever had, and the call runs whatever its class. A yes that is already in the log when the process starts proves nothing, because the process that took it may have run the call and died before recording the result.

| Tool class | Recovery behavior |
|---|---|
| `idempotent` | Re-run with the same key. Safe by contract, checked by the tool's own test. |
| `non-idempotent` | Do not re-run. Append `tool.failed` with `may_have_happened: true` and surface it to the operator. |

A tool declares its class in its manifest. A tool that lies about being idempotent is a bug in the tool, and its test suite must prove the claim.

# Tool manifests

A tool is a directory with a manifest and an executable. The manifest declares what the tool is and what it may touch. The fields that do the real work.

| Field | Controls |
|---|---|
| `class` | Recovery behavior. Idempotent tools may be re-run after a crash, non-idempotent ones may not |
| `risk` | The approval rule. Read and write inside the workspace need no approval, network and destructive calls do |
| `timeout` | The wall clock bound on one call |
| `max_output` | The byte bound on what one event may carry |
| `argv` | The program and its arguments; the model's arguments arrive on standard input |
| `permissions.env` | The environment variable names the process may see. It sees no others |
| `permissions.write` | The only directories the process may write to. Every other path is read-only |
| `permissions.network` | `none` or `any`. There is no third answer |
| `permissions.memory_bytes` | The address space the process may use |
| `permissions.file_bytes` | The largest file it may write |

# The environment a tool gets

A spawned tool inherits nothing. Its environment is exactly the names its manifest declares, resolved from the daemon's own environment at the moment of spawn. A name the daemon holds but the manifest does not declare is not passed.

Inheritance was what the executor did, and it handed every tool the daemon's whole environment, including the provider key that a systemd credential puts there. A tool runs a program the model asked for, and that program's first line of output goes into the log, which is append-only and copied by fork. Inheriting means any tool can copy a credential into the log by accident, and a model able to call a tool can do it on purpose.

`permissions` is required, with no default, the way `risk` is. What a tool may see is the operator's statement, and a default is this program making it for them. `env = []` is a real answer and means the process runs with an empty environment.

A program named without a path is looked up in the daemon's `PATH`, before the child exists. Which program runs is the manifest's statement about `argv`; what it may see is its statement about `permissions`, and one does not decide the other. A tool that declares no environment still starts.

The daemon checks every declared name when it reads the configuration, before it opens the store, and refuses to start when one is not set. The message names the variable and never its value. A manifest that disagrees with its host is a deployment fault, and it should not wait for the first call to surface.

The intent records the names the call had, never the values. The configuration is not in the log and it changes; a reader a year later needs to know which environment this call was given, and that answer has to be in the event.

This is an allowlist, not a sandbox. A tool that declares a name can still print its value, and the log keeps what it prints. The names are the operator's decision about what a tool may know, not evidence that the tool is trustworthy.

# Risk and approval

| Risk | Examples | Rule |
|---|---|---|
| `read` | File read, HTTP GET to an allowlisted host | No approval |
| `write` | File write inside the workspace, git commit | No approval inside the declared paths |
| `network` | HTTP POST to a non-allowlisted host | Approval per host, remembered for the branch |
| `destructive` | Delete, force push, drop table, send a message to a person | Approval per call, never remembered |

Approval is a pair of events. `approval.requested` carries the action summary, the risk class, and an expiry. `approval.decided` carries the decision, the deciding actor, and an optional note. A request that expires is treated as a denial, and the denial is recorded. Nothing waits forever on a human.

Chat surfaces render an approval as a button pair. The TUI renders it as a prompt. The WebUI renders it as a banner. All three send the same command.

# Secrets

No value enters the log. The provider key is named by a variable or a file in the configuration, a tool names the variables it may see, and the value lives in the daemon's environment or in a file the operator controls until the moment the process starts.

An intent records the names, never the values, so a reader can see what a call was given without being able to replay the secret from the log.

A tool that needs a variable its manifest does not declare does not get it, because nothing else is passed to the process at all.

# Output handling

- Captured output under `max_output_bytes` is stored in `tool.result` inline.
- Larger output is written to a blob, and the event records head and tail byte counts plus the truncation flag.
- Binary output is never inlined. It becomes a blob reference with a media type.

A result carries the exit status as data, not as an error. A failing command is a normal outcome the model reads.

# What holds the bounds

A tool is started by a second binary this package ships, `agent-confine`, which puts the bounds on its own process and then becomes the tool. Nothing outside the package is involved: no namespace, no launcher from the distribution, no root.

| Bound | Held by |
|---|---|
| Writing | Landlock refuses every write the manifest did not declare, and truncation with it. A declared directory gets a rule; a path with no rule is refused. The one exception is `/dev/null`, which every program redirects to and which holds nothing |
| Network | seccomp refuses a socket in the address families that reach another machine, so a name lookup, a datagram and a packet socket go the same way |
| Memory | `RLIMIT_AS` |
| One file | `RLIMIT_FSIZE` |
| Time | The manifest's timeout, enforced by the daemon; the process is killed and the log records that the effect may have happened |
| Output | The manifest's byte bound; longer output keeps its ends and the count of what was dropped |
| Environment | The names the manifest declares and nothing else |
| Working directory | The store's directory |

The bounds are syscalls made by the process that is about to become the tool, which is why they hold where a sandbox that needs namespaces does not: a container, a build sandbox, a host with unprivileged user namespaces turned off.

The launcher is a separate binary because a bound has to be installed by the process that will hold it. Installing it in the daemon would bound the daemon, and installing it between fork and exec would mean deciding when it is safe to allocate in the daemon's own child.

A configuration can say `[sandbox] none = true` and get a tool with no bounds at all. It is the only way to ask for that, nothing writes it on the operator's behalf, and the module never does; the table above is then a set of declarations and nothing holds them.

A directory a manifest says a tool may write has to exist before the daemon starts, because Landlock needs the directory to attach the rule to. The daemon refuses a path that is not there, by name; the module creates the paths each tool declares, and nothing creates a directory the program was only given permission to use.

The daemon runs the launcher once at startup, with nothing to become. A kernel that cannot hold a bound is refused there, while somebody is watching the start, rather than on the first tool call.

Not held, and not to be described as held: an allowlist of hosts, because the socket is refused or it is not and a hostname list would be a promise this build cannot keep; CPU time other than the wall clock; and accounting of what a process actually touched. `RLIMIT_AS` bounds address space, not memory in use, so a tool that maps a large file without reading it stays inside the bound and a tool that forks keeps the same ceiling per process.

Do not claim this is a sandbox against hostile code. It is a set of bounds the kernel enforces, and the tools are first-party code the operator installed. A program that is hostile in the sense of exploiting the kernel is out of scope.
