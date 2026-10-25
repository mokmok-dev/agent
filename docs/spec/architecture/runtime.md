---
type: Process Model
title: Runtime
description: Five processes, one writer, a single reactor loop, and a degradation ladder that never loses an accepted event.
tags: [runtime, systemd, launchd, scheduling]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# Processes

| Process | Role | Writes the store |
|---|---|---|
| `agentd` | Reactor, store writer, model and tool gateway | yes, exclusively |
| `agent-web` | HTTP and SSE surface | no |
| `agent-tui` | Terminal surface | no |
| `agent-chat-discord` | Discord surface | no |
| `agentctl` | Operator commands | no, it sends commands |

`agentd` is the only writer, enforced by an exclusive lock on `store/LOCK`. Every other process opens the store read-only and sends commands over a Unix socket at `<runtime-dir>/bus.sock`.

Separate processes buy three things. A surface crash cannot stall the reactor. A surface can be restarted or upgraded without a store restart. A memory leak in a web renderer cannot take the agent down.

# Supervision on Linux

`agent.service`, installed by the flake's NixOS module or by `agentctl install --systemd`. The settings that carry weight, and why.

| Setting | Value | Why |
|---|---|---|
| `Type` with `WatchdogSec` | `notify`, 30 seconds | The ping is sent after a loop iteration completes, so a hang stops the ping and systemd restarts the process |
| `Restart` with `RestartSec` | `always`, 2 seconds | A crash is a restart, not an outage |
| `StartLimitIntervalSec` | `0` | Removes the rate limit, because the persisted backoff inside the agent is the real limiter. Without this, a repeatedly failing machine ends up with a dead unit and no agent |
| `StateDirectory` | `agent`, mode 0700 | Exactly one writable path for a process that otherwise has no write access |
| `ProtectSystem`, `ProtectHome` | `strict`, `true` | The binary and the home directory are read-only to the service |
| `MemoryMax` | 4 GB | A runaway context dies quickly and visibly instead of taking the machine with it |
| `TasksMax` | 512 | Bounds thread and process creation |
| `NoNewPrivileges`, `RestrictNamespaces`, `LockPersonality` | enabled | Removes the cheap escalation paths for a process that runs tools |
| `TimeoutStopSec` | 45 seconds | Longer than the drain bound, so a normal stop never becomes a kill |

# Supervision on macOS

`~/Library/LaunchAgents/dev.mokmok.agent.plist`, installed by `agentctl install --launchd`.

| Key | Value | Why |
|---|---|---|
| `Label` | `dev.mokmok.agent` | Stable identity. |
| `ProgramArguments` | `agentd --state-dir ~/Library/Application Support/agent` | Absolute paths. |
| `RunAtLoad` | `true` | Starts with the agent session. |
| `KeepAlive` | `{ "SuccessfulExit": false }` | Restart on crash, not on a clean stop. |
| `ThrottleInterval` | `10` | launchd's minimum restart gap. |
| `ExitTimeOut` | `45` | Matches `TimeoutStopSec`. |
| `ProcessType` | `Background` | Keeps it out of the foreground QoS class. |
| `StandardOutPath`, `StandardErrorPath` | `~/Library/Logs/mokmok-agent/` | launchd has no journal. |

launchd has no notify protocol and no watchdog. The runtime therefore ships two independent liveness checks for macOS. A `liveness` file is rewritten every 10 seconds by the reactor with the current wall clock and monotonic counters, and `agent-web` exposes a health endpoint that reads it. A separate `agentctl doctor --deadman` invocation, installed as a second `StartInterval` job, kills and lets launchd restart a process whose liveness file is older than 60 seconds.

# The reactor loop

One loop, one writer, one decision at a time.

Each iteration reads the events appended since the last folded position and folds them. It then reads the clock and calls one pure decision function over the folded state and that reading. The decision returns exactly one of four steps. Append a set of events. Sleep until an instant. Wait for a command. Shut down with a reason. The loop carries out that step, then pings the watchdog.

`decide` is a pure function from folded state and a clock reading to one `Step`. It is where the 100 percent killed mutation gate does its most valuable work, because a wrong decision here writes a wrong fact that outlives every restart.

`decide` returns one step, never a loop of steps. The loop re-folds after every append, so a decision never runs on state that a previous decision already invalidated.

# Timers

- Durations use the monotonic clock. Absolute instants use the wall clock and are re-evaluated on each tick.
- A timer fires once even if its due time has passed many times over. The late duration is recorded in `schedule.fired`, and the timer re-arms from now. Catch-up storms after a suspend or a long stall are prevented by construction.
- A wall clock jump beyond 2 seconds in either direction emits `instance.clock_skew` with the measured delta, and timers whose absolute deadline is now in the past fire once.
- No busy polling. The loop blocks on the nearest of the next timer, the socket, and the next store change notice.

# Backpressure and fairness

| Resource | Limit | Behavior at the limit |
|---|---|---|
| Model calls in flight | 4, configurable | Queue per branch, then park the branch |
| Tool calls in flight | 8, configurable | Queue, then park the branch |
| Active branches | 64 | New fork is refused with a clear message |
| Reactor batch | 256 events | Batch closes early |
| Per-turn token budget | configured | Turn fails with `budget_exceeded`, a recorded failure |

A parked branch is state, not a thread. The reactor never blocks on a single branch's work, and one runaway branch cannot starve the others.

# Shutdown

1. `SIGTERM` sets a shutdown flag. The loop finishes the current append.
2. New commands are refused with a `stopping` status.
3. In-flight model and tool calls are cancelled. A cancelled tool call whose effect may already have happened gets a `tool.failed` with `may_have_happened: true`.
4. A snapshot is written for every active branch.
5. `instance.stopping` is appended and synced, the lock is released, and the process exits 0.

`TimeoutStopSec` is 45 seconds, which is longer than the drain bound. A drain that exceeds it is a bug and gets a `SIGKILL` that leaves nothing half-written, because an uncommitted transaction is rolled back.

# Degradation ladder

| Condition | Behavior | Recovery |
|---|---|---|
| Model provider unreachable | Turn fails with a recorded failure, exponential backoff persisted per provider | Next attempt after backoff, no restart needed |
| Provider rate limit | Queue and delay, never retry in a tight loop | Resume when the window clears |
| Tool exits non-zero | `tool.failed` with exit status, the turn continues | The model sees the failure |
| Disk full | Fail stopped, exit 70 | Operator frees space, supervisor restarts |
| Store lock held | Exit 75, no retry storm | The other writer exits or is stopped |
| Clock skew | Emit the event, keep going | Operator notices; timers self-correct |
| Snapshot hash mismatch | Discard the snapshot, fold from an earlier one | Logged, never fatal |
| Corrupt database | Fail stopped, exit 74 | Restore the last backup, then verify on start |
| OOM kill by the cgroup | Restart, recover from the last synced batch | Context shrinking is a follow-up fix |

# Upgrade

`agentd` writes its version and store format into `instance.started`. A binary older than the store format refuses to start. A newer binary migrates forward by appending a migration record, which means a downgrade sees the old format and refuses, rather than misreading.

Rollback is therefore a deliberate act, not an accident.
