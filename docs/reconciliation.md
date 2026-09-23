---
type: Design
title: reconciliation
description: 外部仕様書（AI Coding Agent Architecture Specification）に対する採用/却下の決定台帳。再提案を防ぐため、却下した要件とその理由、仕様との構造的対立、未解決の論点だけを記録する。コードは含まない。
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
document is the ledger of the **decisions** the implementation made about it:
what it deliberately rejects, where it intentionally differs, and what is still
open.

Its job is to stop rejected requirements from being re-proposed as gaps, so it
records only decisions that are still load-bearing. Anything that was built was
deleted from here once it landed: the current state of a component belongs in
[architecture](architecture.md) and the design doc for it, and the history of
building it belongs in the commits. The event and identifier names it uses are
fixed in [vocabulary](vocabulary.md).

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

## Architectural conflicts

These are not simple gaps; each conflicts with a deliberate decision.

1. **Reversibility model.** The specification makes OverlayFS plus unified diffs
   the basis of reversible file operations. The repository deleted the virtual
   filesystem and overlay backends because the executor never used them and the
   OS profile was always the real boundary (`docs/sandbox.md`, "What was
   deleted"). Adopting overlayfs would reintroduce a speculative abstraction the
   design rejected. Reversibility is achievable with an inverse patch instead,
   which is what `agent.patch.applied` carries.

2. **Event store medium.** The specification names SQLite; the implementation
   uses an append-only JSONL log with a hash chain and treats SQLite as a
   rebuildable projection (`docs/architecture.md`, "Log format and recovery
   strategy"). The implementation is the stronger audit and replay design, so
   adopting SQLite as the store would create a second source of truth without a
   benefit.

3. **File editing mechanism.** The specification assumes diff patching; the
   implementation assumes a shell tool. Making the agent's edits structured and
   reversible is a large addition, not a rename. Both exist now: `apply_patch`
   beside `shell`, with the inverse patch in the event.

## Decision record

| Specification objective | Decision | Rationale |
| --- | --- | --- |
| Zero Trust Security | **Already met** by native isolation | bubblewrap, Seatbelt, Landlock, and seccomp enforce the boundary (`docs/sandbox.md`), with `deny > write > read` precedence (`agentd-sandbox/src/policy.rs`). Its reproducibility is met too: `bubblewrap` ships in the Linux dev shell (`flake.nix`). |
| ACID and reversible state | **Reject OverlayFS, adopt a patcher** | OverlayFS contradicts the deleted VFS layer (`docs/sandbox.md`, "What was deleted"); the unified-diff patcher with inverse application was built instead (`agentd-node/src/patch.rs`). |
| Zero-dependency plugin DX | **Reject the engine as specified** | Extensions are out-of-process by design (`docs/architecture.md`, "Extension model") and `Bridge` already covers stdio JSON-RPC tools. |
| Observability and HitL | **Adopted** | `traceparent` propagation and OTLP export are implemented (`docs/telemetry.md`); the approval wait is bounded (`--session-permission-approval-secs`) and the operator has a client (`agentd-approve`). |
| One-command dev experience | **Already met; reframe** | `agentd up` is the one command. `agentd-client` is opt-in and is the second resident component `process-compose` was deferred for (see [client-server](client-server.md)); adopting the orchestrator for that pair is the open decision, not a gap. |
| `PROCESS_PAUSED` | **Reframe** | A child awaiting approval is blocked on a socket or stdio read and consumes no CPU, so a freeze buys nothing; what matters is that the wait always ends. |

## Rejected requirements

These are recorded so they are not re-proposed as gaps.

| Rejected | Reason |
| --- | --- |
| SQLite as the event store | JSONL with a hash chain is the stronger audit and replay store; SQLite already exists as a projection. A second source of truth is forbidden. |
| OverlayFS workspace binding | The OS profile is already the boundary; overlay adds kernel surface the design rejected. |
| Renaming the static allowlist to "dynamic" | A static allowlist plus run-time approval is the same capability; a rename buys nothing. |
| Fixed `/tmp/agent.sock` | XDG runtime paths are private and correct (`agentd-events/src/paths.rs`); a fixed world-known path is a security regression. |
| Pinning the agent to one model | The provider registry already resolves arbitrary models and providers (`docs/inference.md`, "Providers and model routing"). |
| Generic in-process plugin engine | Extensions are out-of-process by design (`docs/architecture.md`, "Extension model"). |
| `PROCESS_PAUSED` as `SIGSTOP` | The wait is already idle, so a freeze only adds signal races against the lifetime kill, the restart budget, and the timeout. |
| "A partial restart must preserve session state" | A daemon restart *cannot* re-adopt a child it did not spawn (the manager's startup reconciliation, `agentd/src/session.rs`), so this reading is unsatisfiable. The satisfiable reading — restart a child while the daemon and the log live on — is the restart budget, which exists. |
| `process-compose` for a single-process daemon | **Deferred, not rejected**: `agentd` alone needs no orchestrator. The client server (`agentd-client`) is a second resident component, so the trigger is met; whether one command for the pair is worth the tool is still open. |

## Open question

- Whether `agentd` and `agentd-client` as two resident components justify
  `process-compose` now that the stated trigger — a second resident component —
  is met, or whether a supervisor is a later answer to a different problem.
