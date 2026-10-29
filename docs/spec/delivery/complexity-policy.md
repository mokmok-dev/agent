---
type: Policy
title: Complexity policy
description: Every function meets a cognitive and a cyclomatic ceiling, measured by cccc, with a baseline that may only shrink.
tags: [complexity, ci, tooling]
status: draft
generated: { by: grok-build, at: 2026-10-29T08:30:00Z }
---

# The rule

`cccc` measures every function in `crates` on every pull request. A function over either ceiling fails the build unless the baseline carries an entry for it, and an entry never grows.

Two numbers describe a function a reader will struggle with. Cognitive complexity counts nesting, so a branch inside three loops costs more than three sibling branches. Cyclomatic complexity counts paths, so it catches a function that hides many branches at one level.

# The ceilings

| Metric | Ceiling | Why |
|---|---|---|
| Cognitive | 15 | SonarSource's rule default. Nesting is what makes a function hard to read, and the ceiling tracks it directly. |
| Cyclomatic | 25 | A second ceiling for branch count where nesting stays shallow. The project's exhaustive matches over the event and operation enums sit above it, so they are listed in the baseline rather than excluded. |

The ceilings are checked with cccc 1.8.5, which the flake builds from its release tag and puts on the dev shell's path.

# The baseline

`nix/complexity-baseline.json` holds the functions already over a ceiling. It only shrinks, and `nix/complexity-gate.sh` enforces each rule.

| Change | Result |
|---|---|
| A new function over a ceiling, with no entry | fails |
| An entry's function grows | fails |
| An entry's function improves | fails, asking for the entry to be lowered |
| An entry's function now passes | fails, asking for the entry to be removed |
| An entry's function no longer exists | fails, asking for the entry to be removed |

An entry is keyed by path and function name. A rename therefore fails until the entry follows it, which keeps the file honest.

| Entry | Measured (cognitive / cyclomatic) | Why it stays |
|---|---|---|
| `core/src/fold.rs` `apply` | 50 / 43 | One flat match over the event taxonomy, with a written allow for its length. Splitting it by event family would put one branch's invariants in two places. |
| `core/src/canonical.rs` `write_data` | 18 / 31 | The same table, writing one arm per event kind into the canonical bytes. |
| `core/src/fold.rs` `sequence` | 18 / 19 | The test generator's match over the operation enum, one arm per operation. |
| `agentd/src/main.rs` `drive` | 17 / 11 | Process wiring, which the mutation policy already excludes as `main.rs`. |
| `model/src/http.rs` `call` | 22 / 11 | The streaming loop. One function owns the read, parse, emit, and end-of-stream order. |
| `runtime/src/config.rs` `parse` | 38 / 29 | One parser and validator for the configuration document. The tool block carries most of the branches. |
| `store/src/lib.rs` `verify_anchors` | 17 / 11 | The hash chain walk, which checks the link, the record, and the anchor in one pass. |
| `xtask/src/spec.rs` `is_timestamp` | 17 / 32 | The docs bundle's timestamp shape check. Every digit position is a condition. |

The baseline covers test code where the code lives in the crate's files. `fold.rs`'s `sequence` is the one entry there, and it is listed rather than hidden.

# The check

`checks.complexity` runs in `nix flake check`, so it runs on every pull request and on `main`. It prints the function count, the maxima, and the ceiling pair, or the failures and a non-zero exit.

Locally, the same gate runs in the dev shell without a build:

```
nix develop --command nix/complexity-gate.sh
```

# What this does not buy

- It does not measure whether a function is tested. Mutation testing answers that, and the two gates are independent.
- It counts match arms as paths, which is why the taxonomy tables sit in the baseline. A table of twenty arms is readable; a nest of twenty branches is not.
- It reads test modules as code. A long table test is allowed to be long, but it needs the same entry as production code.
- It fails on a parse error rather than measuring what the parser recovered. A file cccc cannot read is not a file that passed.

# The iterations

The ceilings and the baseline were chosen by measuring, not by taste. The counts are from cccc 1.8.5 over `crates/`.

| Iteration | What changed | Functions | Max cognitive / cyclomatic | Entries |
|---|---|---|---|---|
| 1 | Measured the tree; no gate yet | 1440 | 50 / 43 | none |
| 2 | Set the ceilings at 15 / 25 | 1440 | 50 / 43 | 9 |
| 3 | Split `core/src/time.rs` `parse` into three phase helpers | 1443 | 50 / 43 | 8 |

`parse` measured 20 / 23 before the split. It now measures 0 / 1, and its helpers measure 7 / 13, 3 / 4, and 9 / 8. The tests and an equivalence run over 194,699 inputs found no behavior change, so the entry was removed rather than lowered.

The remaining entries are the work this gate still owes. Each one is a candidate for the same treatment in its own PR.
