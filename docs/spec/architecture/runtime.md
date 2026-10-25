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

`agent.service`, installed by the flake's NixOS module or by `agentctl install --systemd`.

```ini
[Unit]
Description=Mokmok agent
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=notify
NotifyAccess=main
WatchdogSec=30
Restart=always
RestartSec=2
StateDirectory=agent
StateDirectoryMode=0700
RuntimeDirectory=agent
RuntimeDirectoryMode=0700
ExecStart=/run/current-system/sw/bin/agentd --state-dir /var/lib/agent
KillSignal=SIGTERM
TimeoutStopSec=45
MemoryMax=4G
TasksMax=512
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=true
LockPersonality=true
SystemCallArchitectures=native
```

Four choices in that unit carry weight.

- `Type=notify` with `WatchdogSec=30` means systemd restarts a process that is alive but not making progress. The ping is sent by the reactor only after it completes a loop iteration, so a deadlock stops the ping.
- `StartLimitIntervalSec=0` removes the rate limit, because `RestartSec` plus a persisted backoff inside the agent is the real limiter. Without this, a machine that is failing repeatedly ends up with a dead unit and no agent at all.
- `ProtectSystem=strict` with `StateDirectory` gives the process exactly one writable path.
- `MemoryMax` turns a runaway context into a fast, visible kill rather than an out-of-memory event that takes the machine with it.

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

```rust
loop {
    let batch = store.fold_tail(state.checkpoint_depth)?;   // pure
    let now = clock.now();                                   // port
    let step = decide(&state.apply(&batch), now);            // pure
    match step {
        Step::Append(events) => store.append(&events)?,       // durable
        Step::WaitUntil(instant) => timers.sleep_until(instant),
        Step::WaitForCommand => ipc.next_command(),
        Step::Shutdown(reason) => break reason,
    }
    notifier.ping();                                         // sd_notify
}
```

`decide` is a pure function from folded state and a clock reading to one `Step`. It is where the 100 percent killed mutation gate does its most valuable work, because a wrong decision here writes a wrong fact that outlives every restart.

`decide` returns one step, never a loop of steps. The loop re-folds after every append, so a decision never runs on state that a previous decision already invalidated.

# Timers

- Durations use the monotonic clock. Absolute instants use the wall clock and are re-evaluated on each tick.
- A timer fires once even if its due time has passed many times over. The late duration is recorded in `schedule.fired`, and the timer re-arms from now. Catch-up storms after a suspend or a long stall are prevented by construction.
- A wall clock jump beyond 2 seconds in either direction emits `instance.clock_skew` with the measured delta, and timers whose absolute deadline is now in the past fire once.
- No busy polling. The loop blocks on the nearest of the next timer, the socket, and the store's tail.

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

`TimeoutStopSec` is 45 seconds, which is longer than the drain bound. A drain that exceeds it is a bug and gets a `SIGKILL` that leaves no torn bytes, because the batch boundary is the frame boundary.

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
| Corrupt frame mid-store | Fail stopped, exit 74 | `agentctl doctor --repair` after inspection |
| OOM kill by the cgroup | Restart, recover from the last synced batch | Context shrinking is a follow-up fix |

# Upgrade

`agentd` writes its version and store format into `instance.started`. A binary older than the store format refuses to start. A newer binary migrates forward by appending a migration record, which means a downgrade sees the old format and refuses, rather than misreading.

Rollback is therefore a deliberate act, not an accident.
