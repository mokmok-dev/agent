---
type: Interface Spec
title: Observability
description: Logs for debugging, metrics for trends, events for audit, and a health endpoint that reflects progress rather than liveness.
tags: [observability, metrics, health]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# Three channels, three jobs

| Channel | Answers | Kept where |
|---|---|---|
| Structured log | Why did this process do that just now | journald on Linux, a log file on macOS |
| Metrics | Is the trend wrong | `/metrics` on `agent-web`, Prometheus text format |
| Events | What did the agent actually do | the store, forever |

The log is disposable. The metrics are disposable. The events are the audit record, and they answer "what happened" in a way a log line cannot, because they are ordered, hashed, and replayable.

# Structured log

One JSON object per line on stderr, which journald captures with priority and unit metadata. Fields are `ts`, `level`, `target`, `msg`, plus `branch`, `turn`, `call`, and `correlation` when the line belongs to a unit of work.

Rules that keep the log useful at 3 a.m.

- The correlation id on every line is the turn id, so one `journalctl -u agent -g <turn-id>` returns the whole story.
- No payload content. Prompts, tool output, and model responses live in events and blobs. A log line names the event id instead.
- No secrets, at any level. The redaction list is applied before formatting, not after.
- journald rate limiting is raised in the unit, because a crash loop that logs the same line is exactly when the log matters.

# Metrics

Names follow the Prometheus convention and are defined once, in one table in `agent-runtime`.

| Metric | Type | Definition |
|---|---|---|
| `agent_reactor_loop_seconds` | histogram | Wall time per loop iteration, start to ping |
| `agent_reactor_last_ping_age_seconds` | gauge | Age of the last completed iteration |
| `agent_store_append_seconds` | histogram | Time from batch open to sync return |
| `agent_store_events_total` | counter | Events appended since process start |
| `agent_store_bytes` | gauge | Database, its log, and blobs on disk |
| `agent_store_verify_seconds` | histogram | Time from open to a verified chain |
| `agent_store_chain_failures_total` | counter | Chain verification failures, which must stay zero |
| `agent_model_requests_total` | counter | Labeled by provider, model, and outcome class |
| `agent_model_tokens_total` | counter | Labeled by `input`, `output`, `cached`, `estimated` |
| `agent_model_cache_events_total` | counter | Labeled by `reported` and `hit` |
| `agent_turn_failures_total` | counter | Labeled by failure class |
| `agent_tool_calls_total` | counter | Labeled by tool and outcome |
| `agent_branch_active` | gauge | Branches with pending work |
| `agent_approvals_pending` | gauge | Approvals awaiting a decision |
| `agent_lock_conflicts_total` | counter | Starts refused by an existing writer |

Four of these are the ones worth wiring to an alert on day one. `agent_reactor_last_ping_age_seconds` catches a hang. `agent_store_append_seconds` catches a sick disk. `agent_turn_failures_total` catches a provider problem. `agent_lock_conflicts_total` catches two units fighting.

# Health

`agent-web` exposes two endpoints.

- `GET /healthz` returns 200 when the reactor ping age is under 30 seconds and the store lock is held by the expected process. Anything else returns 503 with a reason string.
- `GET /readyz` returns 200 when every configured projection has caught up to the store tip, and 503 with the lagging projection name otherwise.

The distinction matters. A process can be alive and unable to serve, and a supervisor that only checks liveness will happily keep a stuck process forever.

# Watchdog

The reactor pings systemd only after a loop iteration completes, so the ping means progress, not existence. A decision that blocks forever stops the ping and triggers a restart within 30 seconds.

On macOS the equivalent is the liveness file described in [runtime](runtime.md), checked by `agentctl doctor --deadman`.

# Audit view

`agentctl audit --branch <id> --since <instant>` renders a ledger from events alone. One line per effect, with the tool, the arguments summary, the outcome, the approval that authorized it, and the event id.

This is the view to use when a person asks what the agent did to their account. It is derived, so it cannot be edited, and it cannot drift from what happened.

# Traces

The `traceparent` and `tracestate` extension attributes are passed through when present, and generated when a surface starts a command. A provider call, a tool call, and a follow-up turn stay under one trace, which makes an OpenTelemetry backend usable without inventing a parallel scheme.

A trace is diagnostic. It is not needed to replay history.

# What is deliberately absent

- No per-event log line. A loop that writes one line per event is a loop that logs its own history twice.
- No dashboard in v1. The metrics endpoint plus a local Grafana is a two-minute setup, and building one before there is data is guessing.
- No alert rules in this bundle. Alert thresholds need measured baselines, which do not exist yet. The metrics are defined now so the thresholds can be chosen from data later.
