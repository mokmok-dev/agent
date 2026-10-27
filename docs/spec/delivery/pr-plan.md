---
type: Plan
title: PR plan
description: Fourteen planned units on main, the PRs that delivered them, the units that remain, and the order that keeps every step provable before the next begins.
tags: [delivery, planning]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# How the work stacks

Every PR targets `main`, is independently green, and leaves the tree working. Nothing lands that a later PR has to fix before the tree builds. The order follows one rule. A unit is built only after the unit it depends on can be verified on its own.

Each PR's gate is the same five checks. Format, clippy with warnings denied, the workspace test suite, mutation limited to the changed lines with zero missed and zero timeouts, and the spec check when the bundle changed.

This page holds the plan and its state. A unit names the PR that delivered it, or the work it still owes.

# The units

| # | Unit | Delivers | State |
|---|---|---|---|
| 1 | `docs(spec): open knowledge format bundle` | This bundle | landed, #68 |
| 2 | `ci: gate on mutants, nextest, clippy, and the spec` | Workflows, `xtask spec-check`, `.cargo/mutants.toml` | landed, #69 |
| 3 | `feat(core): ids, timestamps, and the no_std stance` | `agent-core` with the ids and RFC 3339 time | landed, #70 |
| 3b | `feat(core): envelope, taxonomy, and canonical encoding` | The event types, the type table, and the writer that defines an id | landed, #71 |
| 3c | `feat(core): fold` | State, `apply`, invariant errors, `state_hash` | landed, #72 |
| 4 | `feat(store): schema, append-only triggers, chain verification` | `agent-store`, crash tests, `agentctl verify` | partial, #73, #75. `agentctl backup` is declared and not written. |
| 5 | `feat(prompt): segments, prefix chain, cache ledger` | `agent-prompt` and the prefix hash tests | partial, #77, #91. The cache ledger is not written. |
| 6 | `feat(runtime): reactor, timers, supervision` | `agent-runtime`, `agentd`, the systemd unit, sd_notify | partial, #76, #82, #83, #84, #99. The launchd plist is not shipped. |
| 7 | `feat(cli): operator commands` | `agentctl` with `status`, `send`, `verify`, `approve`, `deny` | partial, #86, #94, #95, #96. Thirteen of the nineteen commands fail as unsupported. |
| 8 | `feat(tui): terminal surface` | `agent-tui` | not started, continued as 15 |
| 9 | `feat(web): http and sse surface` | `agent-web` | not started, continued as 18 |
| 10 | `feat(chat): discord surface` | `agent-chat-discord` | not started, continued as 16 |
| 11 | `feat(store): snapshots and blob gc` | A replay budget for long histories | not started, continued as 17 |
| 12 | `harden: sandbox, secrets, approvals end to end` | Bounded tool execution, secret resolution, the approval gate | landed, #83, #92, #93, #97, #98, #99, #101, #102. The bounds are syscalls from `agent-confine`, not a systemd scope. |

The header said twelve until this revision. The table has fourteen rows.

# Where the plan changed

Three things did not go as the plan wrote them.

**The model layer had no unit.** The table went from the fold to the store to the reactor and never scheduled a provider port or an adapter. The port (#78), the OpenAI dialect (#80), the HTTP adapter (#81), and the SSE parser (#79) landed as two crates, `agent-wire` and `agent-model`, each with its own tests. A later fix gave a failure its own words (#100).

**Prompt followed the runtime.** The plan put prompt (unit 5) before runtime (unit 6), because prompt is pure and needs no store. The reactor merged first (#76) and prompt followed (#77). Both orders were verifiable, so the swap cost nothing.

**The tool loop and its gate ran early.** The transcript (#88), the tool intent (#89), the execution (#90), the structured message (#91), the approval gate (#93), recovery (#92), the retry budget (#97), the single-writer lock (#99), the environment allowlist (#101), and the confinement launcher (#102) all landed before any surface. A loop that can run unattended needs the gate and the recovery rules first, so unit 12 could not wait for the chat surface.

# What remains

Four things the definition of done names are not yet true. Nothing accepts a turn from the TUI or from a chat thread. Nothing forks a branch at an earlier turn. Nothing records a cache ledger with reported cached tokens. The first four rows close them, and the rest is work the plan's own units still owe.

| # | Unit | Continues | Why here |
|---|---|---|---|
| 13 | `feat(cli): fork, branch, replay, audit` | 7 | `agentctl fork` is declared and not implemented, and the done sentence about forking has no command behind it. |
| 14 | `feat(prompt): the cache ledger records what the provider reported` | 5 | Nothing records cached tokens yet, and the done sentence asks for them. |
| 15 | `feat(tui): terminal surface` | 8 | The done sentence names the TUI. |
| 16 | `feat(chat): discord surface` | 10 | The done sentence names a chat thread. |
| 17 | `feat(store): backup, snapshots, and blob gc` | 4, 11 | Unit 4 still owes backup and unit 11 owes snapshots and blob gc. One PR over the store closes both. |
| 18 | `feat(web): http and sse surface` | 9 | The browser surface, after the two surfaces the done sentence names. |
| 19 | `feat(model): the anthropic adapter` | new | The second adapter that the [model gateway](../architecture/model-gateway.md) specifies for the first release. |
| 20 | `feat(nix): the launchd plist` | 6 | Unit 6 still owes macOS. |

# Gates beyond the five checks

- A unit that changes supervision runs a boot test on both supervisors. On Linux, `systemd-run --user` is not available in CI, so the test asserts the generated unit against a checked-in expected file, and a manual boot check is recorded in the PR body.
- A surface unit changes an interaction. It carries a screenshot and a short video of the surface driving a real store, and the operator reviews before merge.
- A unit that changes what happens on a dangerous tool call carries an audit-view transcript showing a denied call and an approved call.

# What each PR updates in this bundle

A PR that changes behavior updates the concept that describes it in `docs/spec`, in the same PR. A PR that changes a structural decision adds an entry to [decisions](../architecture/decisions.md) with the alternative that lost. A PR that changes this plan updates this page.

A behavior change with no doc change is a review comment, not a policy someone has to remember.

# Definition of done for the program

The machine runs `agentd` under systemd or launchd, survives a `kill -9` and a reboot, accepts a turn from the TUI and from a chat thread, forks a branch at an earlier turn, and shows a cache ledger with reported cached tokens for the forked turn.

Those five sentences are the acceptance test. Each one maps to a unit above, and each one is checked on a running process, not in a unit test.
