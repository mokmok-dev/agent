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

The implemented subsystems are the WAL record frame, store, and replay in
`agent/src/wal/`, the `CloudEvents` envelope in `agent/src/cloudevent.rs`, the
fan-out broker in `agent/src/broker.rs`, the UDS/WebSocket transport in
`agent/src/transport.rs`, the wire messages in `agent/src/protocol.rs`, the
durable cursor store in `agent/src/cursor.rs`, the wired state machine in
`agent/src/bus.rs`, the authority claim in `agent/src/authority.rs`, the
async connection loop in `agent/src/server.rs`, the sandbox policy core in
`sandbox/src/policy/`, the sandbox filesystem layer in
`sandbox/src/filesystem.rs` and `sandbox/src/executor.rs`, and the sandbox
egress layer in `sandbox/src/egress/` (proxy, forwarder, environment injection,
and transport selection):

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
| `protocol` | A message is classified as an event by `specversion` and as a control message otherwise; every control message round-trips; an unknown type or a non-object is rejected. |
| `cursor` | A cursor survives a reopen as the atomically replaced latest value; cursors are isolated by UID and subscriber ID; `resolve` is `max(requested, stored)`; an unsafe or overlong subscriber ID is rejected; a corrupt file is reported. |
| `replay` | Replay yields records in order from a start sequence, skips earlier records within the starting segment, spans segments, and reports a corrupt segment. |
| `bus` | Publish assigns the bus-owned attributes, commits durably, then fans out; nothing is committed when an envelope is rejected; ack is durable and makes a resume start from `max(requested, stored)`; subscribers of one ID on different UIDs are distinct; a slow subscriber is evicted without blocking publish. |
| `server` | Over a real UDS: publish is committed and acknowledged; subscribe replays history then streams live events; ack persists and the next subscribe resumes; a reserved attribute, a malformed frame, and an ack before subscribe each produce the right error; a disallowed peer is closed; a slow subscriber is closed. |
| `authority` | The decision and rule-change event types require the claim; the `requested` ask and unrelated types do not; a lookalike type is not privileged; only an authority-listener connection may publish a gated type. |
| `server` (close-code) | Every internal disconnect reason maps to exactly one RFC 6455 close frame in one place. |
| `policy::fs` | `deny > write > read` holds: a `deny` inside a broader `write` root wins, an unmatched path keeps the broad read grant, coverage is component-wise, and a `deny` over a write root is a construction error. A `protected` name (`<write-root>/<name>`, default `.git`/`.agents`) is never writable, even with an explicit `write` entry, and an empty `protected` list lifts the cap. |
| `policy` | Every domain rejects its invalid shape (a relative path, a `..` component, a non-component protected name, an empty or whitespace host, a zero port, a zero limit); the workdir must be **effectively writable** (no covering `deny`, not inside a protected name); a policy round-trips through JSON. |
| `filesystem::bwrap` | The argument list grants the broad read root and a private `/dev`, binds each `write` root read-write, re-binds each *existing* protected name read-only, and masks each `deny` **after** the grants (a directory as a read-only tmpfs, a file as the null device); the environment is cleared then set; the scratch is a tmpfs and the `TMPDIR`; namespaces are unshared. When egress grants a Unix socket, the socket's parent is `--dir`-created and the socket bound **read-write** (so `connect` works), with nothing `--dir`-created when there is no grant; a `deny` over the socket or its parent is a `SocketDenied`, and a missing socket a `MissingEgressSocket`. A missing `write` or `deny` target is a `RenderError`. |
| `filesystem` | Backend detection is by capability: a `bwrap` that cannot build a namespace is not selected, a `bwrap` inside a write root is refused, and an empty `PATH` selects nothing. |
| `executor` | The scratch directory is created and removed with its guard. The init argv is the command without egress, and the **supervisor** (`--forward`/`--socket`/`--port`/`--`) with it; egress injects the proxy environment (naming the forwarder's port), and nothing is injected without it. Over a real `bwrap`: a write inside a write root reaches the host; a write outside, a `../` traversal, and a symlink out of the root are all `Read-only file system`; a `deny`d file is unreadable; an existing protected name is read-only; the host environment is scrubbed; the output is capped at the policy limit; a timed-out command is killed with its process group, leaving no marker; and a granted egress socket is visible (and a socket) inside the sandbox. |
| `executor::process` | The long-lived handle: a short process reports its exit code and is not marked killed; a running one reports alive; `kill` stops it and the outcome records the kill (not a clean exit); a finished process reports not running. Over a real `bwrap`: a long-lived confined process runs, and `kill` reaches its descendants (a background writer leaves no marker). |
| `supervisor` | The CLI parser accepts `--forward`/`--socket`/`--port` and the `--` separator (everything after `--` is the command, even a lookalike flag) and rejects a missing or malformed argument. The orchestration, driven by a stub forwarder: the command runs and its code is returned; a forwarder that never reports readiness makes the run fail **without** running the command; and the forwarder is stopped when the command ends. A signal exit maps to `128`. |
| `events` | The classifier recognises the kernel's exact denial messages: `Read-only file system` is a filesystem violation naming the path, `Permission denied` and `Operation not permitted` likewise, and `Network is unreachable` / `No route to host` a network violation with no path. An unrelated non-zero exit yields none; distinct denials are kept and repeats collapse. `exec.completed` carries the exit code, duration, output sizes, and timeout flag; `process.started` names the command and pid; `process.exited` carries the exit code, duration, and killed flag; a violation carries the reason, the path, and a bounded output snippet; `egress.rule_added` / `rule_revoked` carry the `sandbox_id`, `host`, and `port`; the decision events (`requested` / `granted` / `denied` / `cancelled`) carry the `request_id` too; the `traceparent` reaches every event. |
| `egress::destinations` | Match is exact on `host:port`: the host case-insensitively, the port exactly; the empty set permits nothing; a shared prefix and a different port are not matches. `add` is idempotent and `revoke` of an absent rule is a no-op, so replaying a rule log converges; `contains` is exact while `permits` is case-insensitive. |
| `egress::allowlist` | The runtime-mutable set: a rule starts denied, `add` permits it, `revoke` denies it; a revoke closes exactly the tunnels its rule granted (matched case-insensitively on the host, exactly on the port), leaves other destinations' tunnels open, closes every matching tunnel, and does not call a closer for a tunnel the serve loop already deregistered; a poisoned lock denies. |
| `egress::approval` | The consultation rule: a listed destination tunnels, an unlisted one with no approver is refused, and one with an approver is asked. Correlation is strict by `RequestId`: a decision with an unknown or already-resolved id is ignored, a second decision is ignored, and one request cannot release another. An expiry maps to `Cancelled`, **never** `Denied`. `Desk` publishes `requested`, resolves on a correlated decision, and records **its own** `cancelled` only on a deadline (not when the approver cancelled), so exactly one cancellation is recorded. |
| `egress` (approval) | Over a real socket: an unlisted destination with a granting approver is tunnelled and the bytes flow; a denying or cancelling approver is `403`; a listed destination is tunnelled **without** consulting the approver. |
| `egress::request` | The `CONNECT` head parses for a plain, bare-LF, IPv6-bracketed, and header-bearing request; the head length excludes tunnelled bytes; an oversized, incomplete, non-`CONNECT`, malformed, non-UTF-8, portless, zero-port, or headerless request is rejected with the right error. |
| `egress` | Over a real socket: a permitted destination tunnels bytes end to end; a missing or wrong token is `407` without revealing the allowlist; an unlisted destination is `403` and an empty set forbids all; a malformed head is `400` and a failed upstream `502`; the socket is created `0600` and removed on drop; a rule added at runtime permits the next connection, one revoked denies it, and revoking the rule of an **open** tunnel closes it (the client reads EOF). |
| `egress::env` | The four proxy variables are set to a URL naming the reachable loopback port (the forwarder's in the Unix-socket transport, the proxy's in the loopback transport); an operator value is **replaced**, not appended, and the replacement is case-insensitive, so a stale value cannot win; `NO_PROXY` names the command's own loopback; an unrelated variable is untouched. |
| `egress::transport` | Transport follows the host capability: bubblewrap can hold a private network namespace and gets the Unix-socket transport; a host without one is refused (`TransportError::Refused`), so egress fails closed rather than downgrading to the port-only form. |
| `egress::forwarder` | The CLI parser accepts `--socket`/`--port` in either order and rejects a missing, unknown, non-numeric, out-of-range, or zero argument; binding port zero reports a real ephemeral port. Over a real socket: a client's bytes round-trip to the socket and back, a half-close still receives the reply, and the built `egress-forward` binary reports its port and bridges bytes. |

`agent/tests/wal.rs`, `agent/tests/store.rs`, and `agent/tests/cloudevent.rs`
hold the reference-model proptests; `agent/tests/verify_cli.rs` and
`agent/tests/agent_cli.rs` drive the binaries end to end;
`sandbox/tests/policy.rs` holds the policy precedence proptests;
`sandbox/tests/egress.rs` drives the proxy over a real socket with a throwaway
TCP upstream, and `sandbox/tests/forwarder.rs` drives the forwarder over a
throwaway Unix socket, so neither needs confinement; and `sandbox/tests/executor.rs` and
`sandbox/tests/events.rs` spawn a real confined command through bubblewrap,
skipping when the host cannot build a namespace (the condition under which the
daemon refuses to spawn). The `test` flake check puts
`bubblewrap` on `PATH` so those spawn tests run in CI. A GitHub Actions runner
forbids unprivileged user namespaces, so the *spawn* tests skip there even with
bubblewrap present; any logic they would cover has a unit test with a plain
child, which needs no confinement, so no branch loses coverage on those hosts. `agent/src/wal/proofs.rs` holds the Kani harnesses.
A machine-checked obligation catalog is deferred until the verified scope spans
more than one crate; the tables above are the record for now.

The session manager in [docs/session](../session/README.md) is designed and not
yet implemented, so it adds no rows here. When milestone 1 lands, the
`filesystem::bwrap` row gains the `network.unix_sockets` grant, and the
`executor::process` row gains the stdio configuration. Each later milestone adds
its rows as it lands, the same as every milestone before it.

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
