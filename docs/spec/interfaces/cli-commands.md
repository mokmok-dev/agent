---
type: Interface Spec
title: CLI commands
description: The operator command surface, its exit codes, and the JSON mode that makes every command scriptable.
tags: [cli, commands]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# Shape

`agentctl <command> [flags]`. Every command takes `--state-dir <path>`, defaults to `$STATE_DIRECTORY` on Linux and `~/Library/Application Support/agent` on macOS, and accepts `--json` for machine output.

The CLI never writes events. It sends commands to `agentd` over `bus.sock`, or, for read-only commands, opens the database read-only. The exceptions are `gc` and `backup`, which touch files rather than history, and `doctor`, which requires the service to be stopped and says so.

# The configuration file

The daemon reads one TOML file. It holds the state directory, the model, the provider URL, the directive, the tool schemas, the deadline, and the cap on a response. The API key is not in it.

The key is named, not stored: either an environment variable or a file, and exactly one of the two. A key file is what systemd's credential directory makes possible, since the supervisor writes the secret somewhere only the service can read. The daemon reads it at startup and never writes it anywhere, not to the log and not to an event.

Everything is checked before the store is opened. A missing model, a relative URL, a zero timeout, an empty key, or two tools sharing a name stops the process with a message naming the setting. The alternative is a daemon that starts, accepts a command, and only then discovers it cannot reach anyone, which the operator would read as a provider outage.

The file format is TOML rather than YAML, which this repository already reads for the spec bundle. YAML's implicit typing turns an unquoted `on` into a boolean and a version `1.10` into a float, and a configuration file that silently changes its meaning is the opposite of what a startup check is for.

# The daemon's own flags

`agentd` takes a state directory and one flag that ends the process after a single step. The supervisor passes the directory, from `StateDirectory` on Linux and the application support path on macOS. The single-step flag exists so a test can drive one iteration of the loop without a supervisor, and so an operator can run the loop under their own supervision if they prefer it to systemd's.

Nothing else is configurable on the command line. Anything else would be a setting two processes have to agree on, and the file that both read is the store.

# Commands

| Command | Writes | Purpose |
|---|---|---|
| `agentctl status` | no | Branches, their depth and tip time, the approval waiting for an answer, the last failure, store size |
| `agentctl branch list` | no | Branches with depth, label, tip time, last outcome |
| `agentctl branch show <id>` | no | One branch, with the last N turns |
| `agentctl fork` | command | Create a branch at the chosen point |
| `agentctl send --branch <id> <message>` | command | Request a turn |
| `agentctl approve <id>` and `agentctl deny <id>` | command | Decide a pending approval; `--branch` names the branch when the call is on a fork |
| `agentctl archive --branch <id>` | command | Stop scheduling a branch |
| `agentctl warm --branch <id>` | command | Refresh the provider cache for a prefix |
| `agentctl replay --branch <id> [--from <id>] [--to <id>]` | no | Print the folded state transitions as text |
| `agentctl audit --branch <id> --since <instant>` | no | Effect ledger derived from events |
| `agentctl export --branch <id> --out <path>` | no | CloudEvents JSON lines for one branch |
| `agentctl verify [--deep]` | no | Chain, depth, and database integrity verification |
| `agentctl doctor` | no | Diagnose the store and report the failing row |
| `agentctl backup --out <path>` | no | Write a consistent copy while the service runs |
| `agentctl gc [--dry-run]` | yes | Delete unreferenced blobs |
| `agentctl install --systemd` and `--launchd` | yes | Write and enable the unit or plist |
| `agentctl spec check` | no | Validate the OKF bundle under `docs/spec` |
| `agentctl version` | no | Binary version, store format, git describe |

A fork names its parent branch and at most one selector. `--at` takes an event id, `--rewind` takes a count back from the branch's path, and `--turn` takes a turn id; with none of them the fork is at the tip. The selectors read the branch's whole path, ancestors included, so a point on an ancestor makes that ancestor's branch the parent of the new one. `--label` names the branch, `--boundary` moves the fork back to the last prompt segment edge at or before the point, and `--force` accepts a fork point that leaves a tool intent without a result. A fork that would do the latter is refused with exit 65 unless `--force` is given, and the refusal names the call and the tool.

A successful fork prints the new branch id and its depth, and in JSON mode prints `agentctl.fork/1` with `branch` and `depth`. It does not print a transcript: the daemon owns the fold, and the command prints what the append produced.

# Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Usage error |
| 64 | Command failed at runtime |
| 65 | Refused by policy, for example a fork inside an unresolved effect without `--force` |
| 70 | Storage failure, including disk full |
| 74 | Corrupt store, restore from backup |
| 75 | Store lock held by another process |
| 78 | Configuration error |

The codes exist so a script can tell "refused, fix your arguments" from "the disk is full" without parsing text. The same codes come from `agentd`'s own exits, so a supervisor and a script agree.

# JSON mode

`--json` prints one object per invocation, or one object per line for list commands, with a stable field set and a `schema` field naming the shape. An error prints one object under an `error` key holding a code, a class, a message, and a hint, and sets the exit code.

Two rules make the output safe to consume.

- No field is removed and no field changes type within a major version.
- Timestamps are RFC 3339 UTC. Paths are absolute. Event ids are lowercase hex.

# Interactive behavior

The CLI is safe to run from a script, a cron job, a Makefile, and a chat bridge.

- No interactive prompt unless stdout is a terminal and `--yes` is absent.
- `doctor` never repairs. It reports the failing row and the last verified event, and the remedy is a restore from backup. A repair that truncates history would defeat append-only at the exact moment the operator is least able to judge what to drop.
- No command waits indefinitely. `send` and `warm` return after the command is accepted, with the turn or warm id, unless `--wait` is passed.

# The spec command

`agentctl spec check` reads `docs/spec`, and reports three things.

1. Every non-reserved `.md` file parses as YAML frontmatter with a non-empty `type`.
2. Every reserved `index.md` and `log.md` follows its structure, and `okf_version` appears only in the bundle root index.
3. Every internal link resolves, or is reported as a warning, since OKF permits broken links on purpose.

The command is the same check CI runs, so a local pass and a green build mean the same thing. It calls one library function, also exposed as `cargo xtask spec-check` for anyone working in the workspace without a running agent.

# Reading the status

`status` reads the store and does not ask the daemon. The log is the state, so an operator who wants to know what the agent is doing gets an answer whether or not anything is running — and a status that only worked while the daemon was up would be unavailable exactly when it is most wanted.

It does not write: the store is opened read-only, so a status run against a live daemon cannot disturb it and a status run as another user cannot corrupt it.

A missing store is a configuration failure, not an empty status. Nothing has run there, and saying so is different from saying the agent has no branches.

