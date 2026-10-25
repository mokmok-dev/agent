---
type: Policy
title: Mutation policy
description: Every mutant on the logic surface is killed, a surviving mutant fails the build, and an exception needs a written reason in the config.
tags: [testing, mutation, ci]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# The rule

A pull request is mergeable only when `cargo mutants` reports zero missed mutants and zero timeouts on the code it touches. On `main`, the full run over every crate reports the same.

Coverage measures whether a line ran. Mutation testing measures whether a test would notice if the line were wrong. For a store that must never lose a fact and a fold that must never produce a different state from the same events, the second question is the only one worth gating on.

# Scope

| Path | Rule |
|---|---|
| `crates/*/src/**/*.rs` except `main.rs` | 100 percent killed, no exceptions beyond the annotated list |
| `crates/*/src/main.rs` | Excluded by glob. Wiring only, covered by integration tests |
| `crates/xtask/**` | Excluded. Build tooling, not shipped behavior |
| `crates/*/tests/**` | Not mutated. Test code |
| `docs/**` | Not mutated |

`main.rs` is excluded for a specific reason. It constructs concrete adapters, reads arguments, and calls the runtime entry point. Almost every mutant in it either fails to compile or changes nothing observable, and the few that matter are caught by an end-to-end test that starts the process. Requiring `main.rs` mutants would push authors to inline logic there to reach it, which is the opposite of what the architecture wants.

The exclusion is a glob in `.cargo/mutants.toml`, not a convention. A new crate inherits it, and an author who needs a narrower exclusion adds an entry with a reason.

# Configuration

The mutation run is configured with four choices.

| Setting | Value | Why |
|---|---|---|
| Examined paths | Every `src` file in every crate | The logic surface is the whole of `src` |
| Excluded paths | `main.rs` in each crate, and the `xtask` crate | Wiring and build tooling, covered by other tests |
| Test runner | nextest | A process per test isolates a mutant that hangs, and it is faster than a test binary per mutant |
| Timeout | Three times the baseline, with a floor | A mutant that hangs is a failure, not a pass |

The test runner choice matters. A runner that reuses one process lets a hanging mutant stall the whole run, which turns a mutation gate into a timeout.

# Exceptions

There are exactly two kinds.

**An equivalent mutant.** The mutated code produces identical observable behavior, so no test can kill it. It is annotated at the line it appears on, with the reason written in the annotation itself and at least a sentence long. An example is a saturating addition that agrees with a wrapping one because the caller checks the bound before entering.

**Platform-only code.** A branch compiled only on macOS, or only under a feature that CI does not build. It is annotated with the platform and the job that covers it, so a reader knows which CI run stands behind it.

A CI job fails the build when a mutation annotation lacks a reason of at least 20 characters, or when a file adds an annotation without a corresponding line in the reviewed exception list. That check is a script, not a review habit.

# Why the gate is reachable

The gate is only reachable because of the architecture, so treat these as load-bearing rather than stylistic.

- `agent-core` is pure. Time, entropy, network, and disk arrive through ports, so a test substitutes a fixed clock and a seeded generator and drives every branch.
- `decide` is a total function over a closed state and a closed step type. Every arm is reachable from a constructed state.
- The event taxonomy is a closed enum with an exhaustive match. A new variant fails compilation until its fold and its test exist.
- Errors are values. A branch that returns `Err` is a branch a test can reach without a panic hook.
- No `unwrap`, no `expect`, no `panic!` in library code. Clippy denies them at workspace level.
- IO lives in `main.rs` and in adapter crates whose behavior is asserted end to end.

# Test layers

| Layer | Kills mutants in |
|---|---|
| Unit tests per pure function, table driven per event type | fold, decisions, prompt rendering |
| Property tests over generated event sequences | hash chain, depth monotonicity, fold determinism, idempotent fold |
| Store integration tests on a temporary database | schema, triggers, commit, snapshots |
| Fault injection, killing the process mid-transaction and corrupting the file | recovery, chain verification, refusal to start |
| Golden replay fixture, a checked-in database plus its expected state hash | the whole fold pipeline |
| Process tests that start `agentd` and send commands | `main.rs` wiring, the socket, shutdown |

The fault injection tests are the ones that earn their cost. One kills the process between the insert and the commit and asserts that a start finds no event and no half state. Another corrupts bytes in the database file and asserts the store refuses to start rather than serving wrong history. A store that guesses under damage is worse than one that stops.

# CI

| Job | When | Check |
|---|---|---|
| `spec` | every pull request | the OKF bundle check |
| `mutants`, in diff | every pull request | `cargo mutants` limited to the lines the PR changes |
| `mutants`, full | nightly, and on demand | `cargo mutants` over the workspace |
| `clippy` | every pull request | clippy with warnings denied, as a flake check |
| `test` | every pull request | the workspace test suite under nextest, as a flake check |

The in-diff run is the gate authors experience. It mutates only the changed lines, so it stays under a few minutes on a normal PR. The nightly full run catches a mutant that a later change made reachable.

# Reporting

The machine-readable mutation report is uploaded as a build artifact, and the summary line is posted in the pull request. Three numbers are reported, not one: caught, missed, and timeout. Unviable mutants are reported separately and are not a gap in coverage, because a mutant that does not compile cannot be a wrong program.

# What this policy does not buy

- It does not prove the tests assert the right thing. A test that asserts a wrong constant kills mutants and enshrines a bug, which is why property tests and the golden fixture exist alongside.
- It does not cover generated code, and the workspace has none.
- It does not run in the pre-commit hook. A hook that takes minutes gets bypassed, so mutation runs in CI where it cannot be skipped.
