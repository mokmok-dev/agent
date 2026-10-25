---
type: Interface Spec
title: CLI commands
description: The operator command surface, its exit codes, and the JSON mode that makes every command scriptable.
tags: [cli, commands]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# Shape

`agentctl <command> [flags]`. Every command takes `--state-dir <path>`, defaults to `$STATE_DIRECTORY` on Linux and `~/Library/Application Support/agent` on macOS, and accepts `--json` for machine output.

The CLI never writes the store. It sends commands to `agentd` over `bus.sock`, or, for read-only commands, opens the store read-only. The two exceptions are `doctor --repair` and `gc`, which require `agentd` to be stopped and say so.

# Commands

| Command | Writes | Purpose |
|---|---|---|
| `agentctl status` | no | Tip ages, active branches, pending approvals, provider health, store size |
| `agentctl branch list` | no | Branches with depth, label, tip time, last outcome |
| `agentctl branch show <id>` | no | One branch, with the last N turns |
| `agentctl fork --from <branch> (--at <id> \| --rewind <n> \| --turn <id>) [--label <text>] [--boundary]` | command | Create a branch at the chosen point |
| `agentctl send --branch <id> <message>` | command | Request a turn |
| `agentctl approve <id>` and `agentctl deny <id>` | command | Decide a pending approval |
| `agentctl archive --branch <id>` | command | Stop scheduling a branch |
| `agentctl warm --branch <id>` | command | Refresh the provider cache for a prefix |
| `agentctl replay --branch <id> [--from <id>] [--to <id>]` | no | Print the folded state transitions as text |
| `agentctl audit --branch <id> --since <instant>` | no | Effect ledger derived from events |
| `agentctl export --branch <id> --out <path>` | no | CloudEvents JSON lines for one branch |
| `agentctl verify [--deep]` | no | Chain, frame, and depth verification |
| `agentctl doctor [--repair]` | yes | Diagnose a store, and truncate to a named event with explicit confirmation |
| `agentctl gc [--dry-run]` | yes | Delete unreferenced blobs |
| `agentctl install --systemd` and `--launchd` | yes | Write and enable the unit or plist |
| `agentctl spec check` | no | Validate the OKF bundle under `docs/spec` |
| `agentctl version` | no | Binary version, store format, git describe |

# Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Usage error |
| 64 | Command failed at runtime |
| 65 | Refused by policy, for example a fork inside an unresolved effect without `--force` |
| 70 | Storage failure, including disk full |
| 74 | Corrupt store, `doctor --repair` needed |
| 75 | Store lock held by another process |
| 78 | Configuration error |

The codes exist so a script can tell "refused, fix your arguments" from "the disk is full" without parsing text. The same codes come from `agentd`'s own exits, so a supervisor and a script agree.

# JSON mode

`--json` prints one object per invocation, or one object per line for list commands, with a stable field set and a `schema` field naming the shape. Errors print `{ "error": { "code", "class", "message", "hint" } }` to stdout and set the exit code.

Two rules make the output safe to consume.

- No field is removed and no field changes type within a major version.
- Timestamps are RFC 3339 UTC. Paths are absolute. Event ids are lowercase hex.

# Interactive behavior

The CLI is safe to run from a script, a cron job, a Makefile, and a chat bridge.

- No interactive prompt unless stdout is a terminal and `--yes` is absent.
- `doctor --repair` always requires a typed confirmation of the event id it truncates to, or `--yes` plus `--to <id>`.
- No command waits indefinitely. `send` and `warm` return after the command is accepted, with the turn or warm id, unless `--wait` is passed.

# The spec command

`agentctl spec check` reads `docs/spec`, and reports three things.

1. Every non-reserved `.md` file parses as YAML frontmatter with a non-empty `type`.
2. Every reserved `index.md` and `log.md` follows its structure, and `okf_version` appears only in the bundle root index.
3. Every internal link resolves, or is reported as a warning, since OKF permits broken links on purpose.

The command is the same check CI runs, so a local pass and a green build mean the same thing.
