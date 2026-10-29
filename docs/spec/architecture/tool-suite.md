---
type: Design Note
title: The first-party tool suite
description: Six tools, a compiled registry, one workspace, and the changes that make a provider call them.
tags: [tools, design, registry, workspace]
status: draft
generated: { by: grok, at: 2026-10-29T14:44:25Z }
---

# The gap

The loop that turns a provider's tool call into `tool.intent`, an effect, and `tool.result` is complete. Nothing ships that can be called. A fresh install writes `tools = []`, the flake installs no tool, a tool's working directory is the store directory it must never write, and the provider is told about tools in a second system message instead of the `tools` field its dialect defines.

This note designs the first-party suite and the changes it needs. It is a proposal. The policy pages and the PR plan change when the units land.

# What a fresh install gets

The operator names a model, a key, a directive, and a workspace. The module supplies the tools.

```nix
services.mokmok-agent = {
  enable = true;
  model = "deepseek-chat";
  baseUrl = "https://api.deepseek.com/v1";
  apiKeyFile = config.age.secrets.agent-key.path;
  directive = ''
    You work in one directory tree. Every path in a tool argument is relative to it.
    Read before you write. Prefer edit_file over write_file on a file that exists.
  '';
  workspace = "/srv/agent";
  runPrograms = [ "cargo" "git" "nix" ];
};
```

The model receives a `tools` array, answers with `tool_calls`, reads one JSON result per call, and continues the turn. The operator's first command is `agentctl send --branch main "make the failing test pass"`.

# The six tools

| Tool | Class | Risk | Timeout | Event cap | Writes | Network | Environment |
|---|---|---|---|---|---|---|---|
| `read_file` | idempotent | read | 10 s | 64 KiB | nothing | none | empty |
| `list_dir` | idempotent | read | 10 s | 64 KiB | nothing | none | empty |
| `search` | idempotent | read | 30 s | 64 KiB | nothing | none | empty |
| `write_file` | idempotent | write | 15 s | 4 KiB | workspace | none | empty |
| `edit_file` | non-idempotent | write | 15 s | 4 KiB | workspace | none | empty |
| `run` | non-idempotent | write | 900 s | 64 KiB | workspace, tool home, `/tmp` | none | `PATH`, `HOME`, `TMPDIR`, `LANG` |

Every tool process runs under `agent-confine`. The read and write roots are in [Bounds](#bounds). `run` also carries a program allowlist. The other five start no programs.

## The contract

A tool reads one JSON object on standard input and writes one JSON object on standard output. It exits 0 for every outcome it handled, including a missing file or an ambiguous edit, because the model has to read the answer.

```json
{"ok":true,"data":{"path":"src/lib.rs","start_line":1,"end_line":40,"lines":40,"content":"...","truncated":false}}
{"ok":false,"error":{"code":"not_found","message":"src/lib.rs does not exist"}}
```

The code is one of `invalid_args`, `outside_workspace`, `not_found`, `not_a_file`, `not_text`, `too_large`, `ambiguous`, `program_not_allowed`, `io`, or `exec`. A non-zero exit means the tool process failed, not that the model asked for something impossible. The runtime records that status in `tool.result`; only a process that could not start or was killed becomes `tool.failed`.

Paths are relative to the workspace. The tool canonicalizes each path and refuses one that resolves outside the workspace after following symlinks. The kernel's write rules are the second line, and the userspace check gives the model a clear error before the kernel refuses an open.

Each tool measures its serialized result and trims the payload until it fits the event cap. The executor's head and tail cut is a backstop. A cut JSON object would reach the model as broken text.

## The individual tools

`read_file` takes `path`, optional `start_line`, and optional `end_line`. Lines are 1-based and inclusive. The default is the whole file up to 48 KiB of content. Invalid UTF-8 is `not_text`, not a lossy decode. The result reports `start_line`, `end_line`, `lines`, `content`, and `truncated`. The model pages a long file one window at a time, so it sees line numbers it can cite.

`list_dir` takes `path` and optional `depth` from 1 to 3. Symlinks are reported and never followed. `.git` is shown at depth 1 and not recursed into. Entries sort by path. The result stops at 1000 entries or 24 KiB of JSON.

`search` takes `pattern` in Rust `regex` syntax, optional `path`, optional `glob`, `case_sensitive`, and `max_results`. The glob supports `*`, `?`, and `**` and matches the workspace-relative path. The walk skips `.git`, files over 1 MiB, invalid UTF-8, and files with a NUL in the first 8 KiB. The result stops at 200 matches or 24 KiB of JSON.

`write_file` takes `path` and `content`. It creates parents inside the workspace, writes a temporary file in the target directory, fsyncs, renames, and fsyncs the directory. The result reports `path` and `bytes`. A rerun writes the same bytes, so the idempotent class is honest. The content limit is 16 MiB.

`edit_file` takes `path`, `old`, `new`, and optional `all`. `old` is non-empty and must occur exactly once unless `all` is true. Zero occurrences is `not_found`; more than one without `all` is `ambiguous`. The tool rewrites the file atomically and preserves the mode. A rerun after success finds nothing, so the class is non-idempotent. Recovery records that the effect may have happened and does not guess.

`run` takes `program`, `args`, and optional `stdin`. The program must appear in the tool's allowlist. The allowlist is part of the tool's own invocation. The daemon passes it as repeated `--allow=<program>` arguments on `agent-tool run`, so the model cannot widen it from standard input. The tool resolves the program through its declared `PATH`, canonicalizes it, and compares it with each allowed entry resolved the same way. An entry is a program name or an absolute path. A relative entry is refused at startup. An unlisted program is `program_not_allowed`, and the error names the programs that are allowed. The tool execs the canonical path it checked, so the name is not resolved a second time. The program and its arguments go to `execvp` with no shell and no substitution. The working directory is the workspace. The child's environment is exactly `PATH`, `HOME`, `TMPDIR`, and `LANG`, with `HOME` pointing at the tool home. Standard output and standard error are captured separately and each is trimmed to 24 KiB. The exit status is data. A timeout kills the whole process group, because a build starts children of its own.

## Why these six

The five file tools are the read, write, and verify loop every task needs. They cannot write outside the workspace and they cannot read the store.

`run` is what makes real work possible. It builds, tests, formats, and commits. It is a program runner by contract. The allowlist is a policy check in the tool, not a kernel bound. An allowed program can start children of its own, and a build script is arbitrary code. An interpreter on the list makes arbitrary code reachable within the read and write roots. The kernel bounds remain the read roots, the write roots, and the network refusal. Its residual reach is stated in [Bounds](#bounds).

`fetch` is absent from the first release. The network risk is approved once per branch and remembered by tool name, so one yes becomes standing egress to any host. The specification's per-host rule is not implemented. A network tool waits for that rule or for an operator-authored manifest that accepts the broader grant.

A delete tool and a move tool are absent. `run` covers them inside the workspace. A separate destructive tool would ask for per-call approval for a capability `run` already reaches.

An MCP client is absent. It would add a second process lifecycle and a protocol the log cannot bracket with one intent and one result.

# The registry

A new crate, `crates/tools`, holds one description of each first-party tool. It builds a library, `agent-tools`, and one binary, `agent-tool`. The binary dispatches on its first argument.

```rust
pub trait Tool {
    type Args: serde::de::DeserializeOwned + schemars::JsonSchema;
    type Output: serde::Serialize;

    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const CLASS: Class;
    const RISK: agent_core::event::Risk;
    const SCOPE: Scope;
    const LIMITS: Limits;
    const ENV: &'static [&'static str];

    fn run(args: Self::Args, ctx: &Ctx<'_>) -> Result<Self::Output, ToolError>;
}

pub enum Programs {
    /// The tool starts no programs.
    None,
    /// The tool starts only the programs the configuration lists.
    Listed,
}

pub struct RegisteredTool {
    pub name: &'static str,
    pub description: &'static str,
    pub class: Class,
    pub risk: agent_core::event::Risk,
    pub limits: Limits,
    pub scope: Scope,
    pub env: &'static [&'static str],
    pub programs: Programs,
    pub parameters: fn() -> serde_json::Value,
    pub call: fn(&[u8], &Ctx<'_>) -> Vec<u8>,
}

pub fn registry() -> &'static [RegisteredTool];
pub fn select(names: Option<&[String]>) -> Result<Vec<&'static RegisteredTool>, SelectError>;
pub fn tool_block(tools: &[&RegisteredTool]) -> String;
```

The schema comes from `Args` through `schemars`. The parser is the same `Args` through `serde`, with `deny_unknown_fields`. A test per tool passes an extra key and asserts `invalid_args`, and asserts the generated schema says `additionalProperties: false`. The schema and the parser cannot drift because there is one type. The daemon refuses a preset whose registry entry says `Programs::Listed` when the configuration supplies no list, and passes the list on the tool's argv.

The tool block is the exact JSON array the provider receives. It is sorted by name, has no insignificant whitespace, and uses each tool's `parameters` as the function's `parameters`. The root schema loses `$schema` and `title`, because some providers reject them. The same bytes are hashed by the prompt chain and spliced into the request body. One test pins the bytes.

| Fact | Owner | Derived to |
|---|---|---|
| Name, description | `impl Tool` | the tool block, `agent-tool --list` |
| Argument schema | `Args` and `schemars` | the tool block, `--schema` |
| Argument parser | the same `Args` and `serde` | `RegisteredTool::call` |
| Class | `Tool::CLASS` | recovery and the intent |
| Risk | `Tool::RISK` | the approval gate |
| Write scope | `Tool::SCOPE` and the workspace | `Bounds.write`, the systemd unit |
| Read scope | the registry's fixed roots and the workspace | `Bounds.read` |
| Environment names | `Tool::ENV` | the child's environment and the intent |
| Program list | the preset's `programs` and `Programs::Listed` | `Tool.argv`, `tool.intent.programs` |
| Limits | `Tool::LIMITS` | `Tool`, `Bounds` |
| argv | the `agent-tool` path and the name | `Tool.argv` |

The daemon depends on `agent-tools` to resolve a preset. The `agent-tool` binary is found beside `agentd`, or through `MOKMOK_TOOL` for a test or a development build. The executor and the launcher stay in `agent-runtime`, where they are today. The crate map's line for `agent-tools` changes to name the registry and the tool contracts, and the runtime keeps execution and confinement.

# Configuration and the operator's escape hatch

`[[tools]]` stays. An entry either names a preset or carries a full manifest.

```toml
workspace = "/srv/agent"

[[tools]]
preset = "read_file"

[[tools]]
preset = "run"
programs = ["cargo", "git", "nix"]
risk = "destructive"       # the operator may raise the risk, never lower it

[tool_limits.run]
timeout_ms = 1_800_000     # within the registry's 3600 s cap
```

A full manifest keeps the shape it has today.

```toml
[[tools]]
name = "my-tool"
schema = '{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}'
argv = ["/usr/local/bin/my-tool"]
class = "idempotent"
risk = "read"

[tools.permissions]
env = []
network = "none"
memory_bytes = 268435456
file_bytes = 8388608
```

The preset supplies the name, schema, argv, class, risk, environment names, and permission roots. The operator may raise the risk, set the program list, and override the four resource limits within caps. Safety semantics stay with the registry, and the deployment's appetite for risk stays with the operator.

A preset whose registry entry says `Programs::Listed` requires `programs`. The list replaces the module's default. An empty list refuses every program. The daemon writes the list into the tool's argv, and `tool.intent` gains an optional `programs` list stamped from the preset, so the log records what the call could start. The intent also records the model's arguments, so the log shows which program the model asked for. A full manifest takes no `programs` key, because its argv is already the operator's statement.

A full manifest keeps its existing meaning. Two fields become strict. `class` is required, so a missing class no longer means idempotent. `schema` must be a JSON object, because the provider now receives it as a structured parameter schema. The daemon refuses the configuration at startup and names the field to fix.

`workspace` is a top-level setting. It is required when any tool is selected. A bare configuration with no `tools` key still gets no tools, so the daemon keeps its no-default stance. The module writes the six presets by default, so the fresh-install path gets the suite.

# The workspace

The workspace is the one writable root outside the tool home. Every tool starts with it as its working directory. The reactor's current `working_dir`, which is the store's parent, is deleted rather than renamed, because the store must never be a tool's working directory.

The module defaults the workspace to `/var/lib/<state>/workspace` and creates it with tmpfiles. A deployment that wants a different tree sets `workspace = "/srv/agent"` and creates it. The module asserts the path is not `/`, not under `/home`, and not under `/root`, because `ProtectHome = true` masks those paths and `ProtectSystem = strict` makes the rest read-only. The module adds the workspace and the tool home to `ReadWritePaths`.

The daemon creates the workspace only when it lies inside the state directory it owns. Any other path must exist and be a directory at startup, using the existing `MissingDirectory` error.

The tool home is `/var/lib/<state>/tool-home`, mode 0700. `run` uses it for `HOME`, so `cargo` and `git` have somewhere to write. It is outside the store and outside the workspace.

The module writes `[sandbox] none = false` by default and `none = true` when the operator disables the sandbox. The option currently declares a setting that the generated file never contains.

# The provider receives the tools

The renderer owns the bytes. `agent_prompt::render` takes the tool block as one opaque string and hashes it as the tools segment. The block is the exact OpenAI tools array.

The request carries the block as validated `RawValue`. The encoder splices it into `Body.tools` and sends `tool_choice: "auto"`. The second system message disappears. The directive stays the first system message.

```rust
pub struct Request {
    pub model: String,
    pub directive: String,
    pub tools: Option<Box<serde_json::value::RawValue>>,
    pub messages: Vec<Message>,
    pub max_output_tokens: u64,
    pub input_tokens_estimate: u64,
}
```

`Request::of` validates the block as JSON at the boundary and returns an error instead of dropping it. A renderer bug becomes a failed turn, not a model that never calls anything.

The prefix chain keeps its shape. The tools segment now holds the bytes the provider reads, so decision 29's invariant holds by construction. A tool-set change, a description edit, or a schema edit invalidates the tools segment and colds every branch that selects tools. The cache ledger records the provider's reported cached tokens, so the operator can see that one cold prefix. The ledger writer is part of this train.

# Rounds and call ids

Two defects share one cause. `Asked` is overwritten by each response for a turn, and `CallId` is `{turn, step}` with `step` the index inside that response. Round two reuses `turn:0`. The provider sees duplicate tool call ids, and `Branch.decided` can let a round-one approval decide a round-two call. `reopen` compares `MAX_TOOL_ROUNDS` with the latest response's call count, so a model that asks one tool per round never trips the bound.

The smallest fix keeps the operator's `turn:step` grammar and old logs.

`model.responded` gains an optional `base`, the step its first call carries. The runtime computes it as the number of calls already asked in the turn. `Asked` becomes a list of rounds, each with `base`, `requests`, and `filed`. The assistant message projects ids as `base + index`. `next_intent` takes the first unfiled call across rounds. `resolve` finds the round whose range contains the call. `reopen` counts rounds.

An old event has no `base`, so the fold uses base 0 for each round. That reproduces the ids those events were written with, including their historical duplicates. A completed old transcript still renders the same assistant and tool ids. A new event records a monotone base, so ids are unique across the turn.

`tool.failed` also becomes part of the transcript. It files a tool message with deterministic text and reopens the turn. Today a failed call clears `pending` and stops, so the model is never told and the turn is wedged. The text names the call and the error class, and says when the effect may have happened.

# Bounds

`agent-confine` gains read rules beside its write rules. The tool process may read the fixed system roots, the workspace, and the tool home. It may not read the state directory, the credential directory, or the operator's home. Write rules cover the workspace, the tool home, `/tmp`, and `/dev/null`; every other write is refused.

The fixed read roots are `/nix/store`, `/nix/var/nix`, `/run/current-system`, `/etc`, `/proc`, `/dev`, `/tmp`, `/usr`, `/lib`, `/lib64`, `/bin`, and `/sbin`. The workspace and the tool home are added per process. The test builds the tool under `target/debug`, so the test fixture copies `agent-tool` into the workspace or adds that directory to the read roots through a test-only constructor argument. No public knob is added.

The probe at startup installs a read ruleset and a write ruleset with nothing to exec. A host whose kernel cannot hold a read rule is refused while somebody is watching the start.

The daemon sets `PR_SET_DUMPABLE` to `SUID_DUMP_DISABLE` before it opens the store. A non-dumpable process has root-owned `/proc/<pid>` entries, so a same-uid tool cannot read the daemon's environment, its file descriptors, or its memory. That closes the provider key leak without refusing `apiKeyEnv`. The credential file stays outside the read roots.

Two residuals remain, and the design states them.

- A same-uid tool can signal the daemon. `SIGKILL` restarts the service and the turn fails with an intent that has no result. The next start records `may_have_happened` for a non-idempotent call. This is a denial of service, not a disclosure.
- `run` is arbitrary code as the service user. The allowlist stops the model from choosing an unlisted program. An allowed program still starts children of its own, and a build script is arbitrary code. `run` can print any readable file into the log, and the log is append-only and copied by fork. The read roots keep the store and the credentials out of that set. `run` is first-party code the operator opted into, and the operator can remove it or raise its risk to `destructive`.

The suite is not a sandbox against hostile code. It is a set of kernel bounds around tools the operator installed.

A stop during a long `run` reaches `TimeoutStopSec` before the tool finishes. The process is killed with no result, and the next start records `may_have_happened` for the call. A graceful cancel is a later unit.

# Delivery

Each unit targets `main`, is independently green, and passes the standing gate. The distinguishing check is named.

| # | Unit | Changes | Check |
|---|---|---|---|
| 21 | `feat(tools): the registry, six tools, and run's allowlist` | `crates/tools`, `agent-tool`, the golden tool block, per-tool contract tests | `cargo nextest -p agent-tools` |
| 22 | `fix(runtime): a tool's output is drained while it runs` | reader and writer threads, process group, `killpg`, heartbeat callback | `cargo nextest -p agent-runtime a_tool_that_writes_more_than_the_pipe_holds` |
| 23 | `feat(confine): tools read only the roots they need` | read rules, dumpable, probe, kernel tests | `cargo nextest -p agent-runtime --test confine` |
| 24 | `feat(runtime): tools run in a workspace` | `workspace`, preset resolution, `programs` into argv and the intent, `Tool::deployed`, cwd, strict `class` and `schema` | `cargo nextest -p agentd` |
| 25 | `fix(core): call ids are distinct across tool rounds` | `base`, `AskedRound`, `resolve`, `reopen`, `tool.failed` message | `cargo mutants --in-diff` over `fold.rs` |
| 26 | `feat(prompt,model): the provider receives the tools it is told about` | block segment, `RawValue`, `Body.tools`, no second system message | `agentd/tests/configured.rs::the_request_carries_structured_tools` |
| 27 | `feat(runtime): the cache ledger records what the provider reported` | `model.cache.observed` writer | `cargo nextest -p agent-runtime` |
| 28 | `feat(nix): the module writes the workspace, the tool set, and the sandbox` | `runPrograms`, options, TOML, `ReadWritePaths`, `[sandbox]`, package | `nix flake check` |
| 29 | `docs(spec): the suite, the workspace, and structured tools` | this bundle | `cargo xtask spec-check` |

Unit 22 is a prerequisite for `run`. The executor currently waits for the child to exit before it reads standard output, so any tool that writes more than the pipe holds blocks in `write` and is killed at its timeout. The loop also pings the watchdog only after a step, so a tool that runs longer than 30 seconds is restarted mid-effect. The executor drains while it waits and calls a heartbeat, and `run` may take 900 seconds.

# Alternatives rejected

**A closed registry with no operator manifests.** One place for every declaration, and a smaller surface. It removes a capability the specification relies on. The operator's own tool is the escape hatch for a program the suite does not ship, and decisions 31, 36, and 37 describe that operator.

**Per-tool binaries.** Six install lines, six argv substitutions, and the same registry for schemas. It adds surface and hides nothing.

**Manifests generated into the Nix module.** The schema, class, and risk would exist twice, in Rust and in Nix. A drift test catches the divergence, at the cost of a second representation an operator can edit.

**A general shell tool.** The most capable shape. Its residual reach is every program on the host and every file the daemon can read. `run` gives the build, test, and commit path with a structured argv, a program list, and the same kernel bounds.

**A PATH-only policy for `run`.** The operator's `PATH` would be the list. It needs one configuration fewer and every installed program is reachable. It gives the model no statement of what it may use, and an error cannot name the allowed set. The explicit list costs one line and makes the deployment's intent inspectable.

**A kernel execute allowlist.** Landlock can grant `Execute` per file, so the kernel could refuse an unlisted program. A toolchain starts children of its own, and a build script is a file the workspace allows. A complete rule set would need every compiler, linker, and build script, and the list would drift with the toolchain. The tool-level check states what it guarantees, and the kernel still bounds the effect.

**File tools only.** Safe, and unable to build or test. The suite would draft work it cannot verify.

**A network tool now.** One yes becomes standing egress, and the per-host rule is not implemented. Deferred.

**`CallId {turn, round, step}`.** It makes collisions structurally impossible. It also changes the wire id, the operator's `approve` grammar, and the derivation of every old id. The optional `base` achieves the same uniqueness inside one struct.

**An idempotent `edit_file`.** A rerun after a crash cannot tell a successful edit from a missing one without guessing. The log records the class honestly.

# Open decisions

- Is `run` in the module's default set? This note says yes, at `write` risk, because a fresh install that cannot build or test does not complete real work. The operator can remove it or raise its risk to `destructive`.
- Is the default workspace `/var/lib/<state>/workspace` or `/srv/agent`? The first needs no setup. The second is a real working tree and needs one line and one directory.
- Does a tool-set change need a warning before it colds every branch? The cache ledger will show the cold prefix after the fact.
- What default `runPrograms` does the module ship? This note suggests `cargo`, `git`, and `nix`.

# What this does not do

It does not add network tools, a browser, MCP, or a second tool protocol. It does not make `run` safe against a kernel exploit, and it does not bound reads by anything beyond the fixed roots and the workspace. The allowlist is a tool-level check, so an allowed program can start children of its own. It does not merge branches, and it does not change the approval semantics for a future network or destructive tool.
