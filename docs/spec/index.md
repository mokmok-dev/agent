---
okf_version: "0.2"
---

# Product

* [Scope](product/scope.md) - What the agent is for, who operates it, and what it refuses to do.
* [Glossary](product/glossary.md) - The nouns this spec uses, each with one meaning.

# Architecture

* [Overview](architecture/overview.md) - The five layers, the crate map, and the dependency rule that keeps the core testable.
* [Event model](architecture/event-model.md) - The CloudEvents 1.0 envelope, the extension attributes, and the closed event taxonomy.
* [Event store](architecture/event-store.md) - Segment frames, the hash chain, group commit, torn-tail recovery, snapshots.
* [Branching](architecture/branching.md) - Append-only history as a DAG, how a fork is recorded, and why branches never merge.
* [Prompt cache](architecture/prompt-cache.md) - Canonical prompt segments, prefix hashing, the cache ledger, and what the literature does and does not support.
* [Runtime](architecture/runtime.md) - Process model, supervision on systemd and launchd, the reactor loop, backpressure, the degradation ladder.
* [Model gateway](architecture/model-gateway.md) - Provider adapters, streaming, retries, token accounting, cache warming.
* [Tools and effects](architecture/tools-and-effects.md) - Intent and result events, idempotency keys, approval policy.
* [Observability](architecture/observability.md) - Logs, metrics, health, watchdog, and the audit view derived from events.
* [Decisions](architecture/decisions.md) - Each structural choice, the alternative that lost, and why.

# Interfaces

* [UI surfaces](interfaces/ui-surfaces.md) - TUI, WebUI, and chat adapters as read models over one projection.
* [CLI commands](interfaces/cli-commands.md) - The command surface, exit codes, and JSON output.

# Delivery

* [Mutation policy](delivery/mutation-policy.md) - The 100 percent killed rule, its scope, its exceptions, and the CI gate.
* [PR plan](delivery/pr-plan.md) - The stacked PR sequence on `main`, one verifiable unit per PR.

# History

* [Update log](log.md) - Chronological record of changes to this bundle.
