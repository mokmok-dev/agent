---
type: Verification Guide
title: Executable Rust Verification
description: Assess the WAL implementation continuously with cargo-mutants, proptest, and Kani.
tags:
  - rust
  - proptest
  - kani
  - mutation-testing
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

# Executable Rust Verification

The source of truth is the production Rust code. Verification applies the same
properties to the same functions through different search methods; it does not
recreate a state machine inside a test and let the two drift apart.

## Tool Roles

| Tool | Question | What a pass means |
| --- | --- | --- |
| Unit test | Does a known example hold? | The examples that were written. |
| Proptest | Do the properties hold over many generated inputs? | The cases that ran, plus saved regressions. |
| Kani | Is the predicate correct for **all** bit-level inputs? | Every input within the harness's assumptions and unwind bound. |
| cargo-mutants | Which results does no test inspect? | The non-equivalent mutants that were measured were detected. |
| miri | Does `unsafe` violate the aliasing or initialization rules? | The executed paths contain no undefined behavior. |

Kani does not raise the mutation kill rate: its harnesses do not run under
`cargo test`. Its value is orthogonal. cargo-mutants says *where a test is
missing*; Kani says whether the code at that spot is *correct* or merely
uninspected. Do not compare them on one metric.

## Current Scope

The implemented subsystems are the WAL record frame and store in `agent/src/wal/`,
the `CloudEvents` envelope in `agent/src/cloudevent.rs`, the fan-out broker in
`agent/src/broker.rs`, and the UDS/WebSocket transport in
`agent/src/transport.rs`:

| Module | Property |
| --- | --- |
| `frame` | The frame is self-delimiting; encode/decode round-trips; a short buffer is rejected without reading past it. |
| `crc` | The `crc32c` check value matches the Castagnoli vector. |
| `chain` | Records link to the previous record's BLAKE3 hash, from the domain-separated genesis. |
| `scan` | Recovery reproduces a written log, truncates only a torn trailing record, and treats any other checksum or chain failure as fatal. |
| `store` | A reopen reproduces the written payloads and head; rotation preserves the chain across segments; only a torn final segment is truncated; `verify_dir` reports the first failure. |
| `cloudevent` | The `CloudEvents` envelope round-trips; the bus-owned attributes are assigned at commit and a producer-set `source`/`sequence` is rejected; the fixed-width `sequence` encoding preserves numeric order. |
| `broker` | Fan-out reaches every subscriber in order and never blocks; a full queue evicts the slow subscriber with `SlowConsumer`; a dropped or replaced receiver is reaped on the next publish. |
| `transport::allowlist` | A credential is permitted when its UID or its GID is listed; an empty allowlist permits nobody. |
| `transport` | Binding creates a `0700` directory and `0600` socket; a live socket is not stolen but a stale one is reclaimed; the `agent.eventbus.v1` subprotocol is negotiated and an unsupported one is refused; the peer credential is captured. |

`agent/tests/wal.rs`, `agent/tests/store.rs`, and `agent/tests/cloudevent.rs`
hold the reference-model proptests; `agent/src/wal/proofs.rs` holds the Kani
harnesses. A machine-checked obligation catalog is deferred until the verified
scope spans more than one crate; the tables above are the record for now.

Kani is expensive, so its harnesses are kept to the properties only it can
establish: exhaustive bounds safety over attacker-controlled bytes. A property
that a reference model already checks over many inputs — encode/decode
round-trip, chain-link detection, plain length arithmetic — is deliberately
*not* restated as a proof. In particular the `encode_then_decode_round_trips`
harness used to dominate the runtime (hashing a symbolic record makes CBMC
unwind `crc32c` thousands of times) and was removed; its property is covered by
`agent/tests/wal.rs`. When adding a harness, ask what it proves that sampling
does not, and prefer deleting a self-evident one.

Currently verified by Kani:

| Harness | What sampling cannot show |
| --- | --- |
| `a_short_header_is_rejected_without_reading_past_it` | No sub-header-length input reads out of bounds. |
| `a_truncated_payload_past_the_header_is_reported` | No declared-but-absent payload reads out of bounds. |

The proofs run only on **x86_64-linux**. Every supported target is 64-bit, so one
bit-precise run covers the integer semantics, and CBMC's aarch64 backend does not
agree with its x86_64 backend on the recovery path: a scan harness took fifty
minutes and failed on aarch64 while passing in seconds on x86_64. Keeping the
proofs off that backend is cheaper than paying for them on every target.

## Local Checks

```sh
nix develop -c cargo test                       # unit + proptest + doctest
nix develop -c cargo kani -p agent --lib        # Kani harnesses
nix develop -c cargo mutants -j 8 --no-times    # mutation measurement
nix flake check                                 # clippy, nextest, doctest, kani, harness mutation
```

The flake's `test` check runs `cargo nextest`, which does not run doctests,
so a separate `doctest` check runs `cargo test --doc`. Locally, plain
`cargo test` covers both in one invocation.

## Mutation Testing

cargo-mutants rewrites the implementation one spot at a time and reruns the
tests. A rewrite that the tests do not notice is a `MISSED` mutant: no test
inspects that result. Row coverage measures whether a line *ran*; mutation
measures whether its result was *checked*.

Measurement conditions live in `.cargo/mutants.toml`, shared by local runs and
the PR job. A surviving mutant is evidence of a missing test, not of broken
code; Kani separates the two.

Results land in `mutants.out/` (the previous run is moved to `mutants.out.old/`),
both gitignored.

| File | Meaning |
| --- | --- |
| `missed.txt` | Surviving mutants — read this first. |
| `caught.txt` | Mutants a test detected. |
| `unviable.txt` | Mutants that do not compile. |
| `timeout.txt` | Mutants whose tests exceeded the time limit. |
| `diff/` | The rewrite for each mutant. |

Handling survivors:

1. Read the list and record a reason for each equivalent mutant (a rewrite with
   no observable effect). A 100% kill rate is not the goal.
2. Where survivors cluster on a branch, write one property test against a
   reference model — an implementation known to be correct for other reasons,
   such as the in-memory record list in `agent/tests/wal.rs`. One such test can cover
   branches no hand-written case reaches.
3. Do not drop boundary values from a generator. A guard like `if len > 0`
   changes the result only at zero, so a generator that omits zero lets that
   guard's mutant survive.
4. If the input space is small, an exhaustive loop beats sampling.

### Gotchas

- A `#[cfg(kani)]` harness is not compiled by `cargo test`, so a mutant inside
  one always survives. `.cargo/mutants.toml` excludes `agent/src/wal/proofs.rs`;
  harness strength is checked by the harness-mutation derivations in `flake.nix`
  instead.
- The two `recover_from` loop guards only run once, from `position == 0`, so
  `<` → `<=` is unobservable there. `.cargo/mutants.toml` records that
  equivalence as an exclusion.
- Mutants in code excluded by a `#[cfg(feature = "...")]` also survive, because
  it does not compile under the measured build.
- cargo-mutants tests only the mutated package by default. A library function
  that only a downstream consumer exercises survives here; add the test in the
  library.

## Kani Harness Mutation

A Kani harness proves only what it asserts, so a harness whose assertion was
weakened still reports `SUCCESSFUL`. `flake.nix`'s `harnessMutations` injects a
known breakage into production code and requires the named harness to report
`VERIFICATION:- FAILED`. `--replace-fail` and `--harness` make a stale entry
fail instead of passing quietly.

| File | Injected mutation | Harness that must fail |
| --- | --- | --- |
| `agent/src/wal/frame.rs` | `bytes.len() < HEADER_LEN` becomes `< HEADER_LEN - 1` | `a_short_header_is_rejected_without_reading_past_it` |

The mutation lets a header-minus-one buffer fall through the length check, so the
reader proceeds into a header that is not there; the harness that decodes a
symbolic buffer catches it. (A plain `<` → `<=` swap is *not* caught here: it only
changes behavior for a buffer of exactly `HEADER_LEN`, which this harness's
`len < HEADER_LEN` never produces.) Reproduce locally by applying the same edit
and running `nix develop -c cargo kani -p agent --lib --harness <name>`.

Note that Kani does not run `blake3`: the crate reaches `cpuid` inline assembly
for runtime CPU feature detection, which Kani cannot model. The remaining harnesses
therefore decode and validate a buffer without hashing, and the Kani crate list is
confined to `agent/src/wal`. Do not add a harness that hashes unless the hashing
crate stops emitting that asm.

The unwind bound lives in `[workspace.metadata.kani.flags]` in the **root**
`Cargo.toml`, because `cargo-kani` reads flags from the workspace root, not from
the member manifest. Put it in `agent/Cargo.toml` and it is silently ignored:
CBMC runs unbounded, prints thousands of `Unwinding loop ... crc32c` lines, and
eventually reports `VERIFICATION:- FAILED` on an unwinding assertion after tens
of minutes. If a run behaves that way, check where the flags are declared first.

## CI Policy

`nix flake check` (run by the `nix` workflow for every PR and main push)
includes clippy, the nextest suite, the doctest suite, the Kani harnesses, and
the harness-mutation checks.

A PR additionally runs only the mutants produced by its changed lines
(`.github/workflows/nix.yaml`, `cargo mutants --in-diff`), so a newly written
test is measured against the branches it claims to cover. The full measurement
is too slow, and its pass/fail depends on classifying equivalent mutants, so it
stays out of the flake check.

## Deferred

- **miri**: the crate denies `unsafe` (`[lints.rust] unsafe_code = "deny"`), so
  there is no undefined behavior to detect yet. Add it with the first `unsafe`
  or FFI block.
- **Shuttle / loom**: no concurrency code exists yet.
