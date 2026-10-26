# Directory Update Log

## 2026-10-26
* **Update**: Recorded which parser reads the command line, `clap` for `agentctl` and the existing hand-written parser for `agentd`, with the measurements behind dropping `usage-rs`.
* **Update**: Landed the `agentctl` crate that decision 25 describes. `clap` declares the command surface, the parse hands on an `Intent`, and the exit table and the error object stay hand-written. `version` and `verify` answer; the commands that need the daemon wait on its command socket.

## 2026-10-25
* **Creation**: Established the spec bundle with product scope, architecture, interfaces, and delivery sections.
* **Creation**: Recorded the event model, store format, and branch semantics as the three load-bearing concepts.
* **Update**: Removed worked code from every concept. The bundle states policy and shape, and leaves the implementation to the PRs it describes.
* **Update**: Drew the architecture and branch diagrams in Mermaid, so they render and stay legible when the bundle is read in a browser.
* **Update**: Replaced the framed segment format with one SQLite database. The custom format owned durability code that SQLite already provides, and the reversal is recorded in [decisions](architecture/decisions.md).
* **Update**: Strengthened the no-merge decision with the reason that decides it. A prompt is a linear token sequence, and a two-parent history has no canonical linearization.
* **Update**: Promoted every concept from draft to stable after the operator's review, and recorded the review in `verified`.
* **Update**: Named the gate jobs as they are actually implemented, and moved the mutation configuration to `.cargo/mutants.toml`.
* **Update**: Moved the port traits out of the core and recorded the `no_std` stance as a decision, so the constraint is checked by the compiler rather than by review.
* **Update**: Added the event envelope, the type table, and the canonical writer to the core, and recorded that the writer is the only encoder in the system.
* **Creation**: Added the fold concept, which records what state a fold keeps and which contradictions it refuses.
* **Update**: Recorded that the store keeps the core's canonical bytes verbatim, which is what makes verification a hash rather than a second encoder.
* **Update**: Recorded that the store owns the decoder, and why an unknown payload is kept as the bytes it arrived as.
* **Update**: Recorded why error types are hand-written and why the reactor re-folds the whole log on each step.
* **Update**: Recorded the tool block's framing, why the directive version stays out of the bytes, and what the token estimate is allowed to decide.
* **Update**: Recorded where `no_std` applies and why, moved the model port into `agent-model`, and removed a backoff cap that three attempts cannot reach.
* **Update**: Added the wire crate to the crate map and recorded what the server-sent event parser does and does not decide.
* **Update**: Recorded that a request carries its parts separately, and the measured comparison behind choosing `ureq` for the adapter.
* **Update**: Recorded that the HTTP client is in the tree and builds in the sandbox, and what counts as a finished stream.
* **Update**: Added `schedule.armed` to the type table, and recorded the loop's four steps, the wait ordering, and how a turn's input reaches the prompt.
* **Update**: Described the configuration file, what is checked at startup, and why the key is named rather than stored.
* **Update**: Added the installation guide for the NixOS module, and the daemon's heartbeat to the supervision table.
* **Update**: Revised the transcript rule after a second turn lost the first question, and recorded why filing an answer is a decision.
* **Update**: Recorded a tool request with the response that asked for it, and wrote the intent from the log rather than beside it.
* **Update**: Recorded what a tool writes, ran one, and decided how a tool receives its arguments.
* **Update**: Gave a message the structure its role needs, and recorded why the chain hashes it.
* **Update**: Recorded how a call left behind by a dead process is recovered, and why the log cannot answer it.
* **Update**: Recorded the approval gate, the four risk classes, and why the risk has no default.
* **Update**: Recorded the command socket, and why a client does not write to the log.
* **Update**: Recorded that status reads the log rather than the socket.
* **Update**: Recorded that the module installs the operator's command, not only the daemon.

