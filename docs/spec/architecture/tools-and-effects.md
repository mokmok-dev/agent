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
| `argv` | How the arguments are turned into a process |
| `permissions` | The paths, hosts, and environment variables the tool may use, enforced at spawn rather than documented and hoped for |

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

An intent records environment variable names, never values. The executor resolves names from the secret store at spawn time, and the value never enters an event, a log line, or a transcript.

A tool that needs a secret declares it in the manifest. A tool that requests a name not in its manifest is refused before spawn.

# Output handling

- Captured output under `max_output_bytes` is stored in `tool.result` inline.
- Larger output is written to a blob, and the event records head and tail byte counts plus the truncation flag.
- Binary output is never inlined. It becomes a blob reference with a media type.

A result carries the exit status as data, not as an error. A failing command is a normal outcome the model reads.

# Sandboxing by stage

The first release enforces resource limits, an environment allowlist, a working directory restriction, and a timeout. It runs tools as child processes of `agentd`.

A later PR moves tool execution into a transient systemd scope with its own cgroup, filesystem namespace, and network namespace. The manifest fields are designed for that move, so the later change adds enforcement without changing the contract.

Do not claim the first release is sandboxed against hostile code. It is not. It is bounded, and the tools are first-party code the operator installed.
