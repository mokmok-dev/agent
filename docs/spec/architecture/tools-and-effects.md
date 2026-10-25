---
type: Architecture Decision
title: Tools and effects
description: Every external effect is bracketed by an intent and a result, keyed for idempotency, and gated by an approval policy.
tags: [tools, effects, idempotency, security]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# The problem

Replay is safe for state and unsafe for effects. Folding a million events twice produces the same state. Running a shell command twice does not produce the same world.

The store records intent before the effect and result after it, so a crash leaves evidence of what might have happened.

# The pair

```
tool.intent   { call_id, tool, arguments, idempotency_key, expected }
      ... effect happens outside the store ...
tool.result   { call_id, exit_status, duration, stdout_ref, stderr_ref, truncated }
```

or

```
tool.failed   { call_id, error_class, may_have_happened }
```

`may_have_happened` is true when the process had started and the outcome is unknown, for example a kill during execution or a crash before the result was synced.

# Idempotency keys

`idempotency_key = blake3(tool_name || canonical(arguments) || call_id)`.

`call_id` is derived from the turn id and the step index, so a re-fold of the same turn produces the same key. That is what makes the pair replay-stable.

On recovery, an intent without a result is handled by the tool's declared class.

| Tool class | Recovery behavior |
|---|---|
| `idempotent` | Re-run with the same key. Safe by contract, checked by the tool's own test. |
| `non-idempotent` | Do not re-run. Append `tool.failed` with `may_have_happened: true` and surface it to the operator. |

A tool declares its class in its manifest. A tool that lies about being idempotent is a bug in the tool, and its test suite must prove the claim.

# Tool manifests

A tool is a directory with a TOML manifest and an executable.

```toml
name = "shell"
version = 1
class = "non-idempotent"
risk = "write"
timeout_secs = 120
max_output_bytes = 1_048_576
argv = ["sh", "-c", "{{command}}"]
[permissions]
paths = ["{{cwd}}"]
hosts = []
env = ["PATH", "HOME", "LANG"]
```

Four fields do the real work.

- `class` decides recovery behavior.
- `risk` decides the approval rule.
- `paths`, `hosts`, and `env` are enforced at spawn, not documented and hoped for.
- `max_output_bytes` bounds what an event can carry.

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
