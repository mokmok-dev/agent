---
type: Architecture Decision
title: Decisions
description: Each structural choice, the alternative that lost, and the cost we accepted for it.
tags: [adr, decisions]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
sources:
  - id: langgraph-time-travel
    resource: https://docs.langchain.com/oss/python/langgraph/use-time-travel
    title: LangGraph time travel
  - id: temporal-limits
    resource: https://docs.temporal.io/workflow-execution/limits
    title: Temporal workflow execution limits
  - id: restate-durable
    resource: https://docs.restate.dev/concepts/durable_execution
    title: Restate durable execution
---

# How to read this

Each entry names a decision that is expensive to reverse, the alternative that lost, and the cost we accepted. A decision whose cost has not shown up yet is still listed, because the cost is what a future reader needs.

# Summary

| # | Decision | Alternative rejected |
|---|---|---|
| 1 | The append-only log is the only truth | A relational database as truth, projections written to tables |
| 2 | One SQLite database | Framed files in rotated segments |
| 3 | One writer, exclusive lock | Many writers with a broker |
| 4 | Content hash as event id | Random ULID |
| 5 | Canonical JSON (RFC 8785) on disk | CBOR, protobuf |
| 6 | Branch is a path through a single-parent DAG | Per-branch logs, or a merge-capable DAG |
| 7 | No merge | Merge with per-type conflict rules |
| 8 | Tips derived, never stored as truth | A refs file as the source of authority |
| 9 | Surfaces read the store directly | A pub/sub broker as the read path |
| 10 | One process per surface | One process with embedded surfaces |
| 11 | Fail stopped on unwritable storage | Degraded in-memory mode |
| 12 | Provider cache only, no local prompt cache | A local prompt cache in front of the provider |
| 13 | 100 percent killed mutants on the logic surface | Coverage thresholds only |
| 14 | OKF 0.2 for the spec | Plain markdown, or a wiki |
| 15 | Fork appends a new root with a parent pointer | Rewriting or copying history |
| 16 | Snapshots, so replay does not grow with history | Fold from genesis on every start |
| 17 | `agent-core` is `no_std` with `alloc`, and serialization lives at the edge | A `std` core that parses JSON |
| 18 | Errors are written by hand | A derive, as `thiserror` would |
| 19 | The reactor re-folds the whole log on every step | An incremental position cache |
| 20 | The token estimate is four bytes to the token | A tokenizer on the request path |
| 21 | `no_std` marks where the temptation is, not where the code is pure | `no_std` across the workspace, or nowhere |
| 22 | The HTTP client is `ureq`, chosen on a measurement | `reqwest`, or `rustls` directly |
| 23 | A turn's input joins the prompt without becoming a message | Appending a `message.appended` for it |
| 24 | Configuration is one TOML file, checked before anything opens | YAML, which the spec bundle already reads |
| 25 | `agentctl` parses argv with `clap`, and `agentd` keeps its own parser | A table-driven parser in both, or `usage-rs` |

# 1. The log is the only truth

Every derived structure, from the branch tree to the token ledger, is a fold over events. Nothing else may hold state that a fold cannot rebuild.

The rejected design keeps current state in tables and appends an audit row beside each change. It is familiar and fast to query. It also has two write paths that can disagree, and the audit table is usually the one that loses.

Cost accepted. Every query is a fold, so a new view costs code rather than SQL. Mitigated by snapshots and by keeping projections small.

# 2. One SQLite database, not a framed file format

The first draft of this bundle chose length-prefixed frames in rotated segment files. It was reversed before any code was written, and the record stays because the reasoning is what a future reader needs.

What the custom format bought.

- A storage layer a mutation tool can see, since parsing, rotation, and recovery logic would live in Rust where mutants are meaningful.
- A log that is a file, so export is a copy and verification is a scan.

What it cost.

- Every durability property becomes ours to build and to prove. Frame layout and versioning, checksum policy, segment rotation with directory syncs, a tips cache checked against frames, torn-tail truncation, and an archive manifest. Those are the riskiest lines in the system, and the failure mode is silent loss of history.
- Concurrent readers need a protocol we invent.
- Backup needs a consistent copy of an open append-only file, which is a problem in its own right.

SQLite already provides every one of those properties. Crash-safe commits through a write-ahead log, readers that see a consistent snapshot without blocking the writer, a total order, and an online backup that is consistent while the writer runs. What it does not provide is history semantics, and those stay ours. Content hashes, the chain, append-only triggers, the branch DAG, snapshots.

The mutation argument was narrower than it looked. SQL text is opaque to `cargo mutants` under either design, so storage semantics need integration tests regardless. What the gate still covers is the fold, the decisions, and prompt rendering, which is where a silently wrong result is most likely.

Cost accepted. A dependency on SQLite and its Rust binding, a large binary file instead of a readable one, and an export that is a query rather than a copy. In exchange, the code we would have written is code we do not own.

What would reverse it. If the mutation gate turns out to be the only thing catching store defects, or if the log must be readable by a tool that cannot speak SQL. Neither is likely while one process owns the file. The earlier falsifiable trigger never fired, because the code it guarded was never written.

# 3. One writer

A single process appends, enforced by a lock. Readers are unlimited.

The rejected design lets several processes append through a broker, which is how a distributed system begins. It buys throughput we do not need and costs ordering, deduplication, and a second place where history can be lost.

Cost accepted. One process is a restart boundary. The supervisor restarts it in about two seconds, and no accepted event is lost.

# 4. Content hash as id

The id is the hash of the event bytes. Random ids were rejected because a hash makes three problems disappear at once: tamper detection, chain commitment, and duplicate detection on retry.

Cost accepted. `id` changes if any byte changes, including `time`. Building an event twice from the same inputs produces the same id, which is a feature for retries and a surprise for anyone who expects ids to be assigned at write time.

# 5. Canonical JSON on disk

One representation everywhere. The same bytes go into a row, over the socket, and out of an export.

CBOR is smaller and has a deterministic encoding rule. Protobuf is faster to parse. Both add a second representation, and a second representation is a second place for a hash to disagree with a payload.

Cost accepted. JSON is roughly twice the bytes and slower to parse than CBOR. A 30-day history on a personal agent is measured in hundreds of megabytes, which is not a problem worth a codec.

# 6. Branch is a path, not a container

Events are stored once, in one physical log, and a branch is the ancestor chain of a tip.

Per-branch files were rejected because a fork would copy or link history, and both options make the fork either expensive or subtly mutable. A merge-capable DAG was rejected separately, below.

Cost accepted. Reading one branch requires resolving `prev` links, and a branch's events are interleaved with other branches in the file. Both costs are handled by the fold and by snapshots.

# 7. No merge

Merging timelines requires deciding which of two conflicting external effects stands. In an agent runtime, both effects already happened, so no log-based rule can resolve them.

There is a second reason, and it is the stronger one. A prompt is a linear token sequence, and a node with two parents has no canonical linearization. Any merge would have to pick an order, and that order becomes part of every prompt after it, so the same history would render differently depending on the traversal the fold happened to take. The prefix chain would stop being stable, which breaks the one mechanism the branching exists to exploit.

Cost accepted. A winning experiment is promoted forward as new events rather than merged. That loses the ability to keep two branches running and combine them, which no use case in scope needs.

# 8. Tips are derived

The tips cache is checked against the row it points at, and rebuilt on mismatch. The rejected alternative treats a refs file as authoritative, which is how git loses commits after a crash between the object write and the ref update.

Cost accepted. Startup checks each cached tip against its row and rebuilds by query when a check fails. With the cache intact that is one indexed read per branch.

# 9. Surfaces read the store directly

`agentd` is not a query server and not a fan-out broker. Surfaces fold what they need and tail for changes, and the socket carries commands and doorbells.

The rejected design routes reads through the writer. It is simpler for a surface and worse for everyone else, because a slow renderer then holds the writer's attention and a surface cannot answer historical questions at all.

Cost accepted. Every surface links the store crate, so a store bug can crash a surface. That is a crash with no data loss, and the alternative is a bottleneck in the one process that must not stall.

# 10. One process per surface

A surface crash must not stall the reactor, and a leak in a renderer must not take down the writer.

The rejected design puts every surface in one binary. It halves the process count and makes shared state trivial. It also makes a UI panic a full outage.

Cost accepted. More units, more install paths, and one socket protocol to keep stable.

# 11. Fail stopped

When the store cannot be written, the process stops. It never continues from memory.

The rejected design keeps serving from a memory copy and writes when the disk recovers. It produces actions that history does not contain, which is the one failure this architecture exists to prevent.

Cost accepted. Availability is worse during a disk problem. The operator sees a failing unit, which is the correct signal.

# 12. Provider cache only

The prompt is byte-identical to what the provider has seen, so the provider's own prefix cache can hit. A local prompt cache would change the bytes and defeat the match, and would only help a local model.

Cost accepted. Reuse depends on a provider behavior we do not control, so the design measures it instead of assuming it. The ledger is the mitigation. See [prompt cache](../architecture/prompt-cache.md).

# 13. 100 percent killed mutants

Coverage was rejected as the gate because a test that runs a line without asserting anything still counts. Mutation testing at zero missed is the only gate that answers whether a test would notice a wrong line.

Cost accepted. Writing tests to kill every mutant is slower than writing tests to reach every line. On a store that must not lose a fact, it is the cheaper mistake. See [mutation policy](../delivery/mutation-policy.md).

# 14. OKF 0.2 for the spec

This bundle is an OKF 0.2 knowledge bundle. The rejected alternative is plain markdown with a table of contents.

OKF costs a small frontmatter block per concept and buys three things that matter for a spec written by an agent. Every concept declares what it is, so a reader can filter by type. Provenance and freshness are frontmatter fields rather than prose. The index is machine-readable, so `agentctl spec check` and any other consumer can traverse the bundle without a parser per document.

Cost accepted. Frontmatter is duplication when a title repeats the filename, and a reader who does not know OKF sees extra lines at the top of each file. The spec links the format so the next reader can look it up.

# 15. A fork is a new root with a parent pointer

Three durable runtimes solve the same problem the same way. LangGraph forks by appending a checkpoint that carries a `parent_checkpoint_id` and leaves the original thread intact.[^langgraph-time-travel] Temporal resets by copying history up to the reset point into a new execution.[^temporal-limits] Restate's restart-as-new copies a journal prefix into a new invocation id, which is exactly the shape of "same history, different code".[^restate-durable]

All three append rather than rewrite. None of them makes a fork a first-class fact in the log, which is the one thing this design adds, because a first-class fork is what makes the cache ledger answerable per branch.

Cost accepted. The branch tree is a global projection, so a reader of one branch cannot see its children without scanning events from other branches. A copy-on-fork design would avoid the scan and cost a rewrite per fork.

# 16. Snapshots, so replay does not grow with history

The prior art agrees that unbounded history is not a design, it is a deferred incident. Temporal warns at 10,240 events and terminates a workflow at 51,200 events or 50 MB.[^temporal-limits] Restate purges journals after 24 hours by default.[^restate-durable]

This agent runs for years and cannot terminate a conversation, so the bound has to be structural without deleting history. Snapshots are that bound. They do not shrink the database, since no event is ever removed. They bound the work a start has to do, which is the number that would otherwise grow without limit.

Cost accepted. A snapshot format to version and a fold that must stay deterministic, because a snapshot is only useful if the fold that produced it can be repeated and compared. The alternative, folding from genesis on every start, needs no format and makes a cold start proportional to total history, which after a year of running is minutes of CPU before the first command is accepted.

[^langgraph-time-travel]: LangGraph time travel, https://docs.langchain.com/oss/python/langgraph/use-time-travel
[^temporal-limits]: Temporal workflow execution limits, https://docs.temporal.io/workflow-execution/limits
[^restate-durable]: Restate durable execution, https://docs.restate.dev/concepts/durable_execution

# 17. Core is `no_std` with `alloc`, and serialization lives at the edge

The core crate holds the event envelope, the taxonomy, the fold, the decisions, and the canonical encoding. It compiles with `#![no_std]` and `alloc`, and its only dependency is `blake3`. `serde` joins it when the store needs to decode, which is the point at which a decoder exists to justify it.

Two properties follow, and both are the reason for the constraint.

- The core cannot read a clock, open a file, or reach a socket, because the compiler says so. A `std::time::Instant` in a decision function becomes a build error rather than a test that quietly depends on the wall clock.
- Every branch in the core is reachable from data, so the mutation gate is a measurement of the tests rather than of the environment.

Serialization is split deliberately. Core owns the canonical writer, which produces the exact bytes whose hash is the event id. Core never parses those bytes. Decoding a row, and exporting events, happen in the store and the CLI, where `serde_json` lives. The store keeps the canonical bytes verbatim, so verification never re-serializes and there is only ever one encoder in the system.

Cost accepted. `alloc` is required, so this is not bare-metal `no_std`. `HashMap` is out, and `BTreeMap` or a sorted `Vec` stands in, which is an improvement for determinism anyway. Integer formatting, hex, and the RFC 3339 calendar conversions are written here rather than taken from a crate, which is more code to own and more code the mutation gate can measure.

The domain of a timestamp starts at the epoch. A pre-1970 instant from a running service is a broken clock, not history, and excluding it removes a branch from the calendar maths that no test could reach.

# 18. Errors are written by hand, not derived

Every error type implements `Display` and `Error` by hand. `thiserror` would remove about twelve lines per type and is not used.

The gate is the reason. A `Display` match is code `cargo-mutants` can see, and a mutant that replaces the body with `Ok(())` is caught by the test that asserts the message. A derive moves the same text into an attribute, where the gate cannot reach it, and a test that only checks for a non-empty string keeps passing while the messages rot. The boilerplate is bounded, since there are five error types, and none of them chains a source.

The condition to revisit is named in advance. When a third of the error types need `source()`, the hand-written versions become the error-prone part and `thiserror` wins. Until then the std-only form is the smaller thing to own.

# 19. The reactor re-folds the whole log on every step

A step reads every branch, folds it from the root, asks `decide` for one intent, and appends it. There is no incremental position, no cached state, and nothing in memory that a restart could lose.

This is a first cut, and it is slower than the design describes. Two costs are accepted for now. The loop re-folds history it has already folded, so a step costs time proportional to the log rather than to the new events. And the loop sleeps for a fixed interval when there is nothing to do, where the design blocks on the socket and the store change notice, so an idle daemon wakes four times a second and a command waits up to that interval before it is turned into a turn.

The order is deliberate. An incremental fold that is wrong diverges from the log silently, and every fact written after that divergence is wrong in a way no verification can catch, because the log itself is consistent. Re-folding is slow and cannot be wrong. The position cache lands when a measurement shows the cost, which is the point at which a snapshot also becomes worth its format.

# 20. The token estimate is four bytes to the token, and it decides one thing

`agent-prompt` estimates tokens as bytes divided by four, rounded up. There is no tokenizer in the dependency tree, and there will not be one on the request path.

The number is used for exactly one decision: whether a prefix clears a provider's minimum cacheable length, which the literature puts at 1,024 or 4,096 tokens depending on the provider. A four byte token underestimates Latin text and underestimates CJK text by more, and underestimating is the safe direction. A prefix wrongly believed too short sends a request that misses the cache; the opposite mistake records a saving that never happened, and the ledger would then be wrong in the direction that flatters it.

Billing never reads this number. `model.responded` carries the input and output tokens the provider reported.

# 21. `no_std` marks where the temptation is, not where the code is pure

`agent-core` and `agent-prompt` compile with `#![no_std]` and `alloc`. `agent-store`, `agent-model`, and `agent-runtime` do not, and the rule is not that pure code is `no_std`.

The flag buys one thing here: a function that reaches for a clock, a socket, or a file does not compile. That matters most where a branch would quietly start depending on the environment, which is the fold, the decisions, and the renderer. It matters least in code that already takes its inputs as arguments, where a backoff of `(attempt, draw)` cannot read a clock because it was never handed one.

The cost is real and it lands on tests. `vec!`, `format!`, and `ToString` need explicit imports from `alloc`, anything that panics with a string needs a `std` crate under `cfg(test)`, and every dependency must be capable of `no_std`. Paying that inside a crate whose job is to hold a socket client buys nothing at all.

So a crate is `no_std` when it holds logic that is tempting to make environmental, and `std` when its job is to talk to something. `agent-model` is the second case even though everything it holds today is pure, because the adapters that join it will not be.

The same reasoning governs the Sans I/O split, which has not happened yet. When the HTTP adapter lands, the wire half, request bytes out and server-sent events in, becomes its own crate: that is where the branch density is, and where a test needs no server. The socket half stays a thin shell, excluded from the gate by the rule that already excludes a `main.rs`.

# 22. The HTTP client is `ureq`, chosen on a measurement

Three candidates, measured on this machine with a probe crate per option and a clean target directory each: unique crates on normal edges, and seconds for a clean build.

| Option | Crates | Clean build | TLS backend |
|---|---|---|---|
| `ureq` 3 | 28 | 6.2 s | rustls with `ring` |
| `reqwest` 0.12, blocking, rustls | 88 | 6.1 s | rustls with `ring` |
| `rustls` 0.23 directly | 11 | 10.1 s | `aws-lc-rs`, which compiles C |

The build times say almost nothing. The crate counts say a lot, and the third row says the opposite of what it appears to.

`reqwest` is three times the tree of `ureq` and is asynchronous underneath even in blocking mode, so it pulls a runtime into a process whose design is one synchronous loop. The reactor has no executor and no reason to grow one.

`rustls` alone has the smallest tree and the slowest build, and the crate count is a false economy. It is a TLS implementation, not an HTTP client: request framing, chunked bodies, redirects, and connection reuse would all be written here, in the thin shell, where the mutation gate cannot reach them because the code needs a socket. Hand-written HTTP is exactly the kind of code that looks right and fails once a month.

So the adapter uses `ureq`: blocking, small, and correct about HTTP. The cost accepted is 28 crates, one of which builds C code with `ring`, and a dependency whose API can change between major versions.

The C dependency was the risk worth checking before committing, since this project builds in a Nix sandbox where a missing compiler is a build failure rather than a warning. `nix flake check` passes with the dependency in the tree, so `ring`'s C is built by the same toolchain the rest of the workspace uses.

# 23. A turn's input is a transcript entry the fold derives

A turn carries the text the operator asked with. The transcript carries what was said. The prompt needs both, in order, or a second turn sees an answer to a question it cannot find.

The log holds the asking once, as `turn.requested`. The fold projects it into the transcript as a user message at the moment it is applied, which puts it in the right position without a second event. A `message.appended` with the same words would put one utterance in the log twice, and a later reader would have to know which copy to believe.

The cost is a small asymmetry. A message the fold derived has no token count, because nobody counted it, so the field is optional rather than a zero that claims somebody did. The request's own size is recorded on `model.requested`, which is the number that matters for the cache.

An earlier version of this decision kept the input out of the transcript entirely and had the renderer append it per request. That worked for one turn and lost the conversation at the second: the second request carried the first answer with no question in front of it. The test that runs two commands through the daemon found it.

# 24. Configuration is one TOML file, checked before anything opens

The daemon takes a configuration file or a bare state directory. The file holds the state directory, the model, the provider URL, the directive and its version, the tool schemas, the deadline, and the response cap.

Every setting is checked before the store is opened, including the one that needs the outside world: the key. Naming an environment variable that is not set, or a key file that is not readable, stops the process at startup. A daemon that starts, accepts a command and only then discovers it cannot authenticate would present a configuration error as a provider outage, and the operator would spend the evening reading someone else's status page.

The key is named, never stored. An environment variable or a file, exactly one of the two. The file case exists for systemd's credential directory, where the supervisor writes the secret somewhere only the service can read. The daemon reads it at startup and writes it nowhere, not to a log and not to an event.

TOML, not YAML, although this repository already reads YAML for the spec bundle. YAML's implicit typing turns an unquoted `on` into a boolean and `1.10` into a float, and a configuration file that silently changes meaning is the opposite of what a startup check is for. The cost is a parser in the daemon's tree: fifteen crates including `serde`, measured.

# 25. `agentctl` parses argv with `clap`, and `agentd` keeps its own parser

The two binaries face different parses. `agentd` takes a configuration file or a bare state directory and one further flag. `agentctl` takes nineteen leaf commands across two levels of nesting, a global `--json`, and a `fork` that must name exactly one of `--at`, `--rewind`, or `--turn`. One parser is worth owning and the other is not, so the answer is not one framework for the workspace.

`agentd` keeps the parser in `crates/runtime/src/cli.rs`, which is eighty-eight lines of code with ten tests and sits inside the mutation gate. A framework there would add a dependency to a process that [CLI commands](../interfaces/cli-commands.md) gives two flags, and buy nothing.

`agentctl` uses `clap` with the default features off, keeping `std`, `help`, `usage`, `error-context`, and `derive`. That is ten crates on the normal edges, measured with `cargo tree`, against a workspace tree of fifty-eight. Clean dev builds with the compiler cache off cost 1.9 s for the dependency and 4.3 s for `usage-rs`, three runs each on this machine, and both are paid once when someone builds from scratch.

The alternative that lost for `agentctl` is a hand-written table-driven parser. A table of nineteen commands and their flags is not difficult to write. The cost is that every branch of the parser lands on the logic surface, where the gate requires a test that kills each mutant, and those branches are exactly the ones a first draft gets wrong. `--flag=value`, `--` as a terminator, an option given twice, and a group where exactly one member may be present. That is decision 22's shape again, code that looks right and fails once a month, except that here the failure is an operator command that silently parsed into a different command.

`usage-rs` lost for a different reason. Its distinguishing feature is a KDL spec that generates documentation, completions, and manpages, and this repository already has one definition of the command surface, checked by `agentctl spec check` in CI. Adopting it means either two definitions that can drift or moving that table into a second format, which reverses decision 14. Its other advantage is parse speed, which its author measures at 0.19 µs against `clap`'s 480 µs on a 211 command CLI. This binary opens SQLite and folds a log before it prints anything, so that win is not observable here. The cost accepted is eight crates against ten, in exchange for a specification format that nothing else in this repository reads.

What `clap` does not own is the part the interface spec pins down. Parsing produces an `Intent`, which is this project's own enum, never a `clap` type handed further in. The exit code table, 0, 2, 64, 65, 70, 74, 75, 78, and the `--json` error object with its code, class, message, and hint, stay hand-written in a library module where the gate can reach them. Nothing calls `parse()`, which prints and exits. `try_parse()` returns a value, and `main.rs` stays wiring.

Cost accepted. Ten crates in a tree of fifty-eight, and a command declaration that `cargo-mutants` cannot see, because a derive puts it in attributes and the gate reads source. That is decision 18's objection against `thiserror` at a larger scale. The mitigation is the layer [mutation policy](../delivery/mutation-policy.md) already names, process tests that start the binary and assert the exit code and the error object. The gate goes quiet on the declaration rather than failing on it, and that silence is the cost.

The entry was written before the code, in the same order as the rest of the bundle. The parser landed first, as `crates/agentctl`: the declared surface, the conversion to `Intent`, the exit table, and the error object, with `version` and `verify` behind it and every command that needs the daemon waiting on the command socket. PR 7 implements the rest of a choice rather than reopening it.

What would reverse it. A command surface that stops at five commands with no nesting, where the table is smaller than the dependency. Or an objection to the gate going quiet, in which case the builder API declares the same commands in code the gate can read, for four crates instead of ten and roughly three times the lines.

# 26. Filing an answer into the transcript is a decision, not a side effect

`model.responded` records what the provider said. `message.appended` puts it in the transcript. They are separate events, and the second one is chosen by `decide` on the next step rather than written by the code that made the call.

The reason is the window between them. A process that stops after recording the answer and before filing it would leave an answer no later turn can see, and no amount of verification would notice, because the log is consistent: it simply lacks a message. With the reply held in the folded state as something owed, the same loop that repairs a crashed turn files it on restart.

The cost is one more step per turn: the loop writes the answer, then writes the message, then decides again. That is a second fold, and folding is what this design already does after every append.

# 27. A tool request is recorded with the response, and the intent is a decision

The model does not answer with text alone. It can answer by asking for a tool, and those bytes are not in the answer's text, so the response event carries the requests: the tool name and the arguments, unparsed.

The alternative was to write `tool.intent` straight from the call that made the request. That loses the request if the process stops between recording the answer and recording the intent, and the loss is invisible: the log stays consistent and simply lacks a step. With the requests in the event, the intent is derived from what is already durable. `decide` writes one intent per step, and a step already written is not written again, because the call id is the turn and the step.

The idempotency key is a digest of the tool, the arguments, and the call id, separated by a byte that cannot appear in the others. A re-fold of the same turn derives the same key.

An answer with no words in it is not an answer. A response that exists only to ask for a tool files nothing into the transcript, so the conversation does not grow with empty assistant turns.

Two costs. The payload of `model.responded` gained a field, which events written before it do not have: the reader treats it as absent rather than failing, and a reader that predates the field ignores it, so neither side breaks. And the fold refuses the same call filed twice, which only a second writer can do, because the first writer cannot append the same event again.

# 28. A tool is a program, and the arguments arrive on its standard input

The manifest declares one program and its arguments: no shell, no substitution, no pattern language. The bytes the model wrote arrive on the tool's standard input, exactly as it wrote them.

Templating the arguments into a command line would have put a parser between the model and the machine, and every tool would have needed its own escaping rules. Standard input has none: a tool that wants JSON parses JSON, a tool that wants a line reads a line, and a tool that wants nothing ignores it. The cost is that a shell one-liner needs a two-word wrapper rather than a template, which is a smaller cost than a quoting bug in every tool.

The timeout is the manifest's, not the tool's. A process that outlives it is killed and recorded as `tool.failed` with `may_have_happened: true`, because a stopped process may have done half its work. A tool that lies about being idempotent is still a bug in the tool, and the flag is what keeps the log honest about the ones that are honest.

Output longer than the manifest's bound keeps its first half and its last half, with the byte count of what was dropped between them. A program puts its summary and its error at the ends, and the middle of a large output is where the least is.

# 29. A message carries the structure its role needs, and the chain hashes it

A tool conversation is not a list of strings. The answer that asked for a tool carries the calls, with an id and the arguments; the tool's output carries the id it answers. A renderer that kept only role and text could not produce either, and sending the tool output as plain text is refused by every provider that implements the protocol.

So a message holds its role, its text, the calls it asked for, and the call it answers. The directive and the tool block stay as they were. The cost is one optional field pair on the segment, which is also what the encoder reads to decide whether a message is an answer with calls or an output with an id.

The chain hashes the structure along with the text. An earlier sketch of this decision kept the hash on role and text alone, to leave the prefix chain untouched; that was wrong. Two prompts whose only difference is the arguments of a call are different bytes to the provider, so a chain that called them the same prefix would record a warm prefix that is cold, and the cache ledger would report savings that did not happen. The invariant is that the chain describes the bytes the provider reads, and the bytes now include the calls.

A response that asked for tools enters the transcript at fold time rather than through `message.appended`, because the event cannot carry the calls. It enters even when it has no words: an empty answer is not an answer, but an empty answer with calls is the message the results answer.

A round of results reopens its turn so the model can read them, bounded at sixteen rounds per turn. A model that keeps asking for tools would otherwise keep the loop busy forever, and when the bound is reached the results stay in the log, which is a state an operator can read and act on.

# 30. Only the process knows whether it started a call

An intent with no result is ambiguous in the log. It can mean the process died before the effect, or during it, or after it but before the result was written. Nothing in the event says which, because the intent is written before the effect and there is no event between them.

So the runtime decides, and it decides from something the log cannot hold: the set of calls this process filed. A call in that set is one this process asked for and has not yet settled, and running it is the first run. A call outside it has no result and was filed by a process that is gone, which is a recovery, and recovery follows the tool's declared class: an idempotent tool runs again with the same key, and a non-idempotent one does not run, because the log records `tool.failed` with `may_have_happened: true` instead.

The alternative was to write a second event between the intent and the effect, marking the start. It would make the log self-describing, at the cost of an event per call that says nothing an operator needs and that is itself ambiguous when the write is what failed. The set costs one line of process state and cannot be wrong, because a process knows what it filed.

The recovery failure carries the unknown class rather than a new one: what an operator needs to know is that the effect may have happened, and that is the flag, not the class.

# 31. What may run unattended is a line in the configuration, and there is no default

A tool declares its risk: read, write, network, or destructive. Read and write inside the paths the manifest declares run without asking. Network is approved once per branch and remembered. Destructive is approved every time and never remembered. A question that goes unanswered past its expiry is a denial, recorded as one, so nothing waits forever on a person.

The risk has no default. A default would be this program deciding what a stranger's tool may touch, and every plausible default is wrong for somebody: read lets a destructive tool run silently, destructive makes a first install refuse to do anything. The operator writes one word per tool, the daemon refuses to start without it, and the module has no default either.

The gate lives in the runtime rather than in `decide`, because it needs two things the folded state does not hold: the tool's declared risk, which is configuration, and the answer to "has this process already asked". `decide` says which call is next; the runtime says whether the effect may happen. The decision itself is a pure function over the declaration and the branch's log, so it is tested without a process.

The command that answers a question is the same command channel that carries a message: `approve` or `deny`, with the call id as its text. That is not a temporary shape. The alternative was a dedicated socket for approvals, which would be a second way into the same log, and the log is the one place a second way has already caused trouble.

