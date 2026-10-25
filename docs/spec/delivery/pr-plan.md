---
type: Plan
title: PR plan
description: Twelve stacked pull requests on main, each ending in a green gate, in an order where every step is provable before the next begins.
tags: [delivery, planning]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# How the work stacks

Every PR targets `main`, is independently green, and leaves the tree working. Nothing lands that a later PR has to fix before the tree builds. The order follows one rule. A unit is built only after the unit it depends on can be verified on its own.

Each PR's gate is the same five checks. Format, clippy with warnings denied, the workspace test suite, mutation limited to the changed lines with zero missed and zero timeouts, and the spec check when the bundle changed.

# The sequence

| # | Title | Depends on | Delivers |
|---|---|---|---|
| 1 | `docs(spec): open knowledge format bundle` | none | This bundle |
| 2 | `ci: gate on mutants, nextest, clippy, and the spec` | 1 | Workflows, `xtask spec-check`, `.cargo/mutants.toml`, nextest in the flake |
| 3 | `feat(core): ids, timestamps, and the no_std stance` | 2 | `agent-core` skeleton with the ids and RFC 3339 time, 322 mutants killed |
| 3b | `feat(core): envelope, taxonomy, and canonical encoding` | 3 | The event types, the type table, and the one writer that defines an id |
| 3c | `feat(core): fold` | 3b | State, `apply`, invariant errors, `state_hash` |
| 4 | `feat(store): schema, append-only triggers, chain verification` | 3c | `agent-store`, crash tests, `agentctl verify`, `agentctl backup` |
| 5 | `feat(prompt): segments, prefix chain, cache ledger` | 3c | `agent-prompt`, ledger fold, prefix hash tests |
| 6 | `feat(runtime): reactor, timers, supervision` | 4, 5 | `agent-runtime`, `agentd`, systemd unit, launchd plist, sd_notify |
| 7 | `feat(cli): operator commands` | 6 | `agentctl` with `status`, `fork`, `send`, `replay`, `audit`, `verify` |
| 8 | `feat(tui): terminal surface` | 7 | `agent-tui` |
| 9 | `feat(web): http and sse surface` | 7 | `agent-web` |
| 10 | `feat(chat): discord surface` | 7 | `agent-chat-discord` |
| 11 | `feat(store): snapshots and blob gc` | 6 | Replay budget for long histories |
| 12 | `harden: sandbox, secrets, approvals end to end` | 10 | Scope-backed tool execution, secret resolution |

# Why this order

PR 3 before PR 4 because the store cannot be tested without an event type to write, and the event type cannot be tested without a fold that consumes it. The core landed in three pieces rather than one, because the ids and the calendar are where a 100 percent killed gate is either reachable or not, and finding that out first was cheaper than finding it out after the event types were written.

PR 5 sits beside PR 4 rather than after it because prompt rendering is pure and its tests need no store. Landing it early makes the cache ledger available before the first real model call, which means the ledger has data from the first day of running.

PR 6 is the first PR that can be left running. Everything after it is a surface or an optimization, so the machine can be handed over at that point and the rest lands while it runs.

PR 11 is late on purpose. Snapshots only matter once history is long, and writing them before there is history means writing them without a realistic test fixture.

# Gates beyond the five checks

- PR 6 and later run a boot test on both supervisors. On Linux, `systemd-run --user` in CI is not available, so the test asserts the generated unit against a checked-in expected file, and a manual boot check is recorded in the PR body.
- PRs 8, 9, and 10 change an interaction. Each carries a screenshot and a short video of the surface driving a real store, and the operator reviews before merge.
- PR 12 changes what happens on a dangerous tool call. It carries an audit-view transcript showing a denied and an approved call.

# What each PR updates in this bundle

A PR that changes behavior updates the concept that describes it in `docs/spec`, in the same PR. A PR that changes a structural decision adds an entry to [decisions](../architecture/decisions.md) with the alternative that lost.

A behavior change with no doc change is a review comment, not a policy someone has to remember.

# Definition of done for the program

The machine runs `agentd` under systemd or launchd, survives a `kill -9` and a reboot, accepts a turn from the TUI and from a chat thread, forks a branch at an earlier turn, and shows a cache ledger with reported cached tokens for the forked turn.

Those five sentences are the acceptance test. Each one maps to a PR above, and each one is checked on a running process, not in a unit test.
