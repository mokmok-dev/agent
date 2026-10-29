# Directory Update Log

## 2026-10-30
* **Update**: Updated the draft tool suite design with `run`'s program allowlist, recorded in the intent.

## 2026-10-29
* **Update**: Added a draft design for the first-party tool suite, the workspace, and the structured tools field.
* **Update**: Added the complexity policy. cccc measures every function, the ceilings are 15 cognitive and 25 cyclomatic, and the baseline of existing offenders may only shrink.
* **Update**: Split `Timestamp::parse` into date, fraction, and offset helpers and removed its baseline entry.

## 2026-10-27
* **Update**: The mutation policy names the property tests that now exist: canonical encoding uniqueness, the fold's tip and depth, fold determinism, and the state a rejected event leaves behind.
* **Update**: Recorded the delivered PRs in the PR plan, and the work each partial unit still owes. The page had not changed since unit 3 landed.
* **Update**: Added the units that remain, each one saying which planned unit it continues, and corrected the plan's count from twelve to fourteen.

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
* **Update**: A failure says whether another attempt is coming, not whether the error looks retryable. Past the budget the two parted, and the loop asked a refusing provider 840 times in eight seconds.
* **Update**: A yes written by the current process is the first yes a call waiting on a question ever had, so answering after a restart runs it instead of claiming the effect may have happened.
* **Update**: The daemon locks the store it writes, so a second agentd on the same state directory exits instead of doubling every turn.
* **Update**: A failure records what it said about itself, bounded and without the request, so `transport` is no longer the end of the diagnosis.
* **Update**: A tool's process sees the variable names its manifest declares and nothing else, and the intent records the names it was given.
* **Update**: A tool's bounds are syscalls its own process makes, through a launcher this package ships, so they hold on a host that will not give up namespaces. `[sandbox] none = true` is the one way to say otherwise.
* **Update**: A fork appends one `branch.created` whose predecessor is an event on the parent branch, and the child reads its inherited transcript from that path instead of copying it. The store learned to anchor a branch's first event, and verification checks the edge and the payload that names it.
* **Update**: The command socket answers a fork after the append rather than when the line is read, because the new branch id and a policy refusal are results of the append. `send` now carries the branch it belongs to, so a turn on a forked branch lands on that branch.
