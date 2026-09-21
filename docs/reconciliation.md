---
type: Design
title: reconciliation
description: 外部仕様書（AI Coding Agent Architecture Specification）と本リポジトリ実装の乖離台帳。各要件を第一原理で再審査し、一致・表層差・意図的逸脱・欠落・再定義に分類して採用/却下を記録する。コードは含まない。
tags:
  - reconciliation
  - architecture
  - spec
  - design
generated:
  by: human
  at: 2026-09-21T00:00:00Z
---

# agentd spec reconciliation

An external specification ("AI Coding Agent Architecture Specification") describes
a deterministic, reversible, highly isolated AI coding agent platform. This
document is the ledger of how the implementation in this repository relates to
it: which requirements it already meets by a different mechanism, which it
deliberately rejects, which are genuinely missing, and which differ only in
surface detail.

It contains no code. Its job is to fix the adopt/reject decisions before any
implementation starts, so that the missing pieces are built and the rejected
ones are not resurrected.

**Written against `c8ca532`.** The citations below are line numbers in that
tree, so they go stale as the code moves; §[Re-verification](#re-verification)
lists the commands that re-establish every claim.

## Method

Every requirement is taken apart to its fundamental claim and rebuilt from
there, then run through the five-step design process:

1. **Make the requirement less dumb.** Is the requirement necessary, or can it
   be simpler?
2. **Delete the part or process.** What can be removed? "Just in case" is
   forbidden.
3. **Simplify or optimise.** Do not optimise something that should not exist.
4. **Accelerate cycle time.** Only after the direction is settled.
5. **Automate.** Last, never first.

The specification is treated as a requirement of unknown quality, not as an
authority: several of its components were already considered and rejected inside
this repository's own design docs, and those decisions are cited as evidence.

## Status legend

| Status | Meaning |
| --- | --- |
| **Aligned** | Met, by the same mechanism or an equivalent one. |
| **Cosmetic** | Met, but with a different name, path, or transport. No action. |
| **Deviation (intentional)** | The repository chose a different design on purpose; the decision is documented. |
| **Missing** | No design decision either way; the feature simply does not exist. |
| **Reframe** | The requirement names a mechanism that conflicts with the architecture; the underlying goal needs a different shape. |

## Component matrix

### A. Isolation and security layer

| Specification | Implementation | Status |
| --- | --- | --- |
| Host root bound read-only | `bwrap --ro-bind / /` (`agentd-sandbox/src/linux.rs:328-330`) | Aligned |
| Workspace on OverlayFS (`LowerDir` original, `UpperDir` diff) | Plain `--bind` of the workdir (`agentd-sandbox/src/linux.rs:340-342`); no overlayfs anywhere. The VFS/Overlay layer was deleted (`docs/sandbox.md:218-226`) | Deviation (intentional) |
| Writes to unnecessary system paths physically blocked | Protected names re-bound read-only (`agentd-sandbox/src/linux.rs:347-349`), `deny` entries masked after the grants (`agentd-sandbox/src/linux.rs:352-363`), and a Landlock allowlist fallback that **fails closed** when a `deny` is not expressible (`agentd-sandbox/src/linux.rs:542-558`) | Aligned (stronger) |
| `--unshare-net` | `--unshare-all` drops the whole network namespace (`agentd-sandbox/src/linux.rs:325`) | Cosmetic (stronger) |
| Only `/tmp/agent.sock` mounted | The one socket is the policy's `network.unix_sockets` path under `$XDG_RUNTIME_DIR/agentd` (`agentd-events/src/paths.rs:44-51`); Linux bind-mounts it into the private namespace (`agentd-sandbox/src/linux.rs:370-376`) and `agentd-egress-forward` bridges it to loopback (`agentd-sandbox/src/forward.rs`) | Cosmetic |
| Dynamic allowlist check | Static `host:port` allowlist plus a run-time approval (`agentd/src/proxy.rs:112-149`) | Cosmetic |
| HitL when a domain is not allowlisted | `session.egress.requested` / `granted` / `denied`, correlated by `request_id`, a timeout denying (`agentd/src/proxy.rs:31-35`, `155-198`) | Aligned |
| (Not in the specification) kernel syscall filtering | The Landlock helper installs a seccomp filter as well (`agentd-sandbox/src/helper.rs:139`, `248`), which the specification does not ask for | Beyond spec |

### B. Atomic and reversible state layer

| Specification | Implementation | Status |
| --- | --- | --- |
| Unified diff parser and `apply` | Absent. The agent's only editing mechanism is the `shell` tool (`agentd-node/src/agent.rs:502-520`, `604-620`); the workspace manifest carries no diff/patch crate | Missing |
| `apply_reverse` (invert `-`/`+` and hunk headers) | Absent; there is no undo mechanism of any kind | Missing |
| Fuzzy and recount match (hunk-header repair) | Absent | Missing |
| Atomic dry-run, all-or-nothing | Absent | Missing |
| `CodePatchAppliedEvent` | Absent (follows from the missing patcher) | Missing |
| OverlayFS integration | Absent, for the same reason as the workspace row | Deviation (intentional) |

### C. Event sourcing and observability layer

| Specification | Implementation | Status |
| --- | --- | --- |
| CloudEvents 1.0 envelope | `Event` with `SPEC_VERSION = "1.0"` and `DAEMON_SOURCE` (`agentd-events/src/lib.rs:96`), plus a `prevhash`/`chainhash` tamper-evident chain (`agentd-events/src/chain.rs:21-26`) | Aligned (stronger) |
| Event store in SQLite | Append-only JSONL (`agentd-events/src/log.rs`); SQLite is the node's rebuildable projection and is never the store (`docs/architecture.md:303-305`) | Deviation (intentional) |
| `traceparent` (W3C Trace Context) in extensions | Implemented: parse/validate (`agentd-events/src/trace.rs`), envelope extension and ingress validation (`agentd-events/src/lib.rs:176`, `agentd-events/src/lib.rs:248`), normalisation on append (`agentd/src/server.rs:583-586`), the proxy links the child's `CONNECT` context and stamps the approval events (`agentd/src/proxy.rs:155-198`, `594-600`), and the agent stamps its own publishes (`agentd-node/src/agent.rs:595-596`) | Aligned |
| OTel / Jaeger span export | OTLP/HTTP export with link-don't-adopt semantics (`agentd-telemetry/src/lib.rs`, `agentd-telemetry/src/semconv.rs`); the dev shell ships `opentelemetry-collector` with `nix/otelcol.yaml` because Jaeger is not in nixpkgs (`docs/telemetry.md:93-98`) | Cosmetic (any OTLP endpoint) |
| `PROCESS_PAUSED` freeze while awaiting approval | No `SIGSTOP`/`SIGCONT`/pause of any kind | Reframe (see Rejected) |
| A bound on the approval wait | Egress waits under a timeout and then denies (`agentd/src/proxy.rs:155-198`); `session.permission.requested` has **no timeout at all**, so a pending ACP permission is answered never (`agentd/src/bridge.rs:428-439`) | Missing |
| A surface for the human to answer with | Any `authority` client may decide (`docs/egress.md:294-312`), but no approver binary ships: only `agentd-agent`, `agentd-node`, and `agentd-publish`, and the last needs a hand-written `--type`/`--data` | Missing |

### D. stdio plugin interface

| Specification | Implementation | Status |
| --- | --- | --- |
| stdio / NDJSON / JSON-RPC | The `Protocol`/`Bridge` split (`agentd/src/bridge.rs:107`), `McpProtocol` as newline-delimited JSON-RPC 2.0 (`agentd/src/bridge.rs:159`), `AcpProtocol` as a state machine with id correlation (`agentd/src/bridge.rs:284`) | Aligned |
| Ephemeral mode and daemon mode | `Sandbox::exec` (`agentd-sandbox/src/sandbox.rs:126`) versus `Sandbox::spawn`/`SandboxedProcess` (`agentd-sandbox/src/sandbox.rs:178`, `agentd-sandbox/src/sandbox.rs:284`) under the manager's supervision (`agentd/src/session.rs:79-95`, `agentd/src/session.rs:215`) | Cosmetic |
| Language- and SDK-free plugin engine | No plugin registry or manifest; extensions are out-of-process CloudEvents clients by design (`docs/architecture.md:307-314`) | Reframe |

### E. Dev environment and orchestration

| Specification | Implementation | Status |
| --- | --- | --- |
| `flake.nix` makes the dependencies portable | The dev shell ships `skills`, the Rust toolchain, `sccache`, and `opentelemetry-collector` (`flake.nix:100-115`). **`bubblewrap` is absent**, and a policy that needs a private network namespace fails closed without it on Linux (`agentd-sandbox/src/linux.rs:85`, `agentd-sandbox/src/linux.rs:458-492`) | Missing (bubblewrap) |
| `process-compose.yaml` and `nix run .#dev` | No `process-compose` reference anywhere in the repository, and no `apps` output in `flake.nix` | Reframe (see Rejected) |
| Jaeger started as a process | `opentelemetry-collector` is started by hand; see the export row above | Cosmetic |
| One-command launch | `agentd up --workdir` (`agentd/src/up.rs`), reachable as `nix run .` through `meta.mainProgram = "agentd"` (`flake.nix:95`) | Aligned (different shape) |
| TUI partial restart (hot reload) preserving session state | No watcher or reload mechanism; a child restarts only within the supervision budget (`agentd/src/session.rs:79-95`, `agentd/src/session.rs:215-437`), and a *daemon* restart fails every open session (`agentd/src/session.rs:298`) | Reframe (see Rejected) |

## Architectural conflicts

These are not simple gaps; each conflicts with a deliberate decision.

1. **Reversibility model.** The specification makes OverlayFS plus unified diffs
   the basis of reversible file operations. The repository deleted the virtual
   filesystem and overlay backends because the executor never used them and the
   OS profile was always the real boundary (`docs/sandbox.md:218-226`). Adopting
   overlayfs would reintroduce a speculative abstraction the design rejected.
   Reversibility is achievable with an inverse patch instead.

2. **Event store medium.** The specification names SQLite; the implementation
   uses an append-only JSONL log with a hash chain and treats SQLite as a
   rebuildable projection (`docs/architecture.md:303-305`). The implementation is
   the stronger audit and replay design, so adopting SQLite as the store would
   create a second source of truth without a benefit.

3. **Observability was the real gap, and it is closed.** Unlike the two above,
   `traceparent` and OTel export had no counter-decision anywhere, so they were
   built (Phase 1, below). What remains is not the mechanism but its usability:
   the approval wait has no bound and the human has no client.

4. **File editing mechanism.** The specification assumes diff patching; the
   implementation assumes a shell tool. Making the agent's edits structured and
   reversible is a large addition, not a rename.

## Decision record

| Specification objective | Decision | Rationale |
| --- | --- | --- |
| Zero Trust Security | **Already met** by native isolation | bubblewrap, Seatbelt, Landlock, and seccomp already enforce the boundary (`docs/sandbox.md:19-25`), with `deny > write > read` precedence (`agentd-sandbox/src/policy.rs:6`). Only the reproducibility of `bubblewrap` is missing. |
| ACID and reversible state | **Reject OverlayFS, adopt a patcher** | OverlayFS contradicts `docs/sandbox.md:218-226`; a unified-diff patcher with inverse application is a real gap and is kept as backlog. |
| Zero-dependency plugin DX | **Reject the engine as specified** | Extensions are out-of-process by design (`docs/architecture.md:307-314`) and `Bridge` already covers stdio JSON-RPC tools. |
| Observability and HitL | **Adopt; mechanism done, usability missing** | `traceparent` propagation and OTLP export are implemented. The approval wait needs a bound and the operator needs a client. |
| One-command dev experience | **Already met; reframe** | `agentd up` is the one command. `process-compose` is deferred until a second resident component exists. |
| `PROCESS_PAUSED` | **Reframe** | A child awaiting approval is blocked on a socket or stdio read and consumes no CPU, so a freeze buys nothing; what is missing is that the wait always ends. |

## Rejected requirements

These are recorded so they are not re-proposed as gaps.

| Rejected | Reason |
| --- | --- |
| SQLite as the event store | JSONL with a hash chain is the stronger audit and replay store; SQLite already exists as a projection. A second source of truth is forbidden. |
| OverlayFS workspace binding | The OS profile is already the boundary; overlay adds kernel surface the design rejected (`docs/sandbox.md:218-226`). |
| Renaming the static allowlist to "dynamic" | A static allowlist plus run-time approval is the same capability; a rename buys nothing. |
| Fixed `/tmp/agent.sock` | XDG runtime paths are private and correct (`agentd-events/src/paths.rs:29-51`); a fixed world-known path is a security regression. |
| Pinning the agent to Gemma | The provider registry already resolves arbitrary models and providers (`docs/inference.md:141-152`). |
| Generic in-process plugin engine | Extensions are out-of-process by design (`docs/architecture.md:307-314`). |
| `PROCESS_PAUSED` as `SIGSTOP` | The wait is already idle, so a freeze only adds signal races against the lifetime kill, the restart budget, and the timeout. Reframed into the two adopted items below. |
| "A partial restart must preserve session state" | A daemon restart *cannot* re-adopt a child (`agentd/src/session.rs:298`), so this reading is unsatisfiable. The satisfiable reading — restart a child while the daemon and the log live on — is the supervision budget, which already exists. |
| `process-compose` for a single-process daemon | **Deferred, not rejected**: there is no second resident component to orchestrate. Revisit when one exists (Phase 5). |

## Adopted backlog

| Phase | Scope | Priority | Status |
| --- | --- | --- | --- |
| 0 | This document: the adopt/reject record | — | Done |
| 1 | Observability: `traceparent` propagation and OTLP export | High | Done (`71dd695`, `f6b4c10`, `b969086`, `c8ca532`) |
| 2 | Isolation reproducibility: ship `bubblewrap` in the dev shell | High | Open |
| 3 | Approval usability: bound the wait, ship an approver client | High | Open |
| 4 | Unified-diff patcher: parse, `apply`, `apply_reverse`, recount, dry-run | Medium | Open |
| 5 | Deferred: `process-compose`, a plugin registry, remote child restart | Low | Not started |

No phase starts until its predecessor is agreed.

### Phase 2 detail: bubblewrap in the dev shell

- Add `pkgs.bubblewrap` to the dev shell packages (`flake.nix:100-115`).
- Do **not** add Landlock tooling: the repository applies Landlock and seccomp
  with its own `agentd-sandbox-helper` (`agentd-sandbox/src/bin/`), so no
  external tool is involved.
- Do **not** add Jaeger: it is not in nixpkgs, and any OTLP endpoint already
  serves (`docs/telemetry.md:93-98`).
- Validate with `nix develop -c bwrap --version`, the bubblewrap-detecting tests
  (`agentd-sandbox/tests/egress_flow.rs`), and a `nix run . -- up` launch whose
  backend selection is bubblewrap rather than the helper fallback.

### Phase 3 detail: approval usability

- Bound the wait: when `session.permission.requested` is published, start a
  deadline and publish `session.permission.decided` (`cancelled`) on expiry,
  mirroring the egress timeout. The deadline belongs to the stateful side — the
  `SessionManager` (`agentd/src/session.rs`), not the pure `AcpBridge` — and a
  late human decision stays harmless because an unknown `request_id` is ignored
  (`agentd/src/bridge.rs:539`).
- Ship `agentd-approve` beside `agentd-publish`: subscribe with a read token,
  print the pending `sandbox.permission.requested`, `session.permission.requested`,
  and `session.egress.requested` events, and publish the decision (granted /
  denied / cancelled) under the same `request_id` with the `authority` token.
- Validate by driving a confined real agent through a permission round trip and
  answering it from the CLI, and by holding an egress `CONNECT` open while
  answering it.

### Phase 4 detail: patcher

- Unified-diff parse, `apply`, and `reverse` with hunk-header inversion, plus
  deterministic recount repair for LLM-generated headers.
- In-memory dry-run with all-or-nothing application, then an atomic write.
- Expose it as an agent tool alongside `shell` (`agentd-node/src/agent.rs`), and
  emit `agent.patch.applied` carrying the applied files, the line counts, and the
  **inverse patch**, so undo is "read the inverse from the log and apply it" and
  no daemon-side patch store is needed.
- Apply against the existing read-write bind; no overlayfs.

## Re-verification

Re-establishing the ledger from a clean tree:

| Claim | Command |
| --- | --- |
| OverlayFS, patcher, pause, `process-compose` are absent | `rg -i 'overlay\|lowerdir\|apply_reverse\|sigstop\|process-compose' --glob '!target'` |
| `bubblewrap` is absent from the flake | `rg -n bubblewrap flake.nix`, `nix develop -c bwrap --version` |
| The permission wait has no timeout | `rg -n 'timeout\|Timeout' agentd/src/bridge.rs` (no match) |
| No approver binary ships | `ls agentd-node/src/bin` |
| The tree is green | `cargo test --workspace --all-features --no-fail-fast` |

## Open questions

- Phase 3: should `up` give `--session-permission-approval-secs` a default, or
  keep an unbounded wait the operator must opt out of? An unbounded default is
  what lets an agent hang forever today.
- Phase 4: is one `apply_patch` tool enough beside `shell`, or is the intent to
  narrow `shell` later? Today the patcher is additive, not a boundary.
- Phase 4: confirm the inverse patch belongs in the event payload (undo from the
  log alone) rather than in a side store.
- Phase 5: what concrete trigger ends the deferral — a second resident
  component, a second plugin, or a second host platform?
