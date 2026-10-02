---
type: Design
title: agent
description: The purpose-built agent image, the session it runs as, and the events it owns
tags:
  - session
  - agent
  - image
  - eventbus
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-29T00:00:00Z
---

## The Agent, the Image, and the Session

Three words name three different things, and the design keeps them apart:

- An **agent** is the program this project builds. It is a Rust crate in this
  workspace. It subscribes to the bus, decides what to do, and publishes what it
  did.
- A **session** is one live agent process, with one workspace, one confinement
  policy, and one egress allowlist, and a lifetime the daemon owns.
- An **agent image** is a template a session is created from. It carries the
  policy builder, the program, and the output contract.

An image is data. A session is an instance. A program is code. A second kind of
agent is a second image, not a second session manager.

The workspace already has a crate named `agent`, and it is the event bus, not the
program. This document set calls the program the **agent**, in the sense of
[vision.md](../vision.md), and it names the bus crate `agent` only when it means
the crate.

## Why a Purpose-Built Agent

An earlier direction ran a third-party agent program inside the sandbox and
forwarded its standard input and output over the bus. That direction is dropped,
on two grounds.

- A third-party agent needs a terminal. Its useful interface is a screen, and a
  screen over a bus is a PTY multiplexer, which is a large feature built to
  serve a program this project does not control.
- A third-party agent hides its decisions. Its output is prose. The durable log
  this project already keeps becomes unreadable if the only record of a session
  is a terminal transcript.

A purpose-built agent removes both problems. Its input is a bus subscription, so
there is no stream of bytes to forward and no terminal to emulate. Its output is
events it authors, so the log records what it did in a shape a program reads.

The cost is that the agent is new code. The benefit is that it is the same code
the tests exercise, in the same language as the two crates it stands between.

## The Agent Runs as a Bus Peer

The agent reaches the bus over a Unix socket, and the daemon publishes nothing
on the agent's behalf. The agent holds its own subscription, its own durable
cursor, and its own connection.

This is what deletes a request router from the design. A session is already
identified by its `subject` on the bus, so the agent can subscribe to
`agent.session.<id>.command` and publish `agent.session.<id>.output`. The manager
never sits between the agent and its input. It starts the peer and stops the peer.

The consequence is that the agent's input and output are durable by construction.
A command that arrives while the agent is restarting is in the log, and the
agent's cursor replays it. Nothing is lost between the client and the agent.

Milestone 5 implements this as the `agentd` crate: a
[contract](#the-output-contract) of two event types, and a loop that connects,
subscribes as `agent-<id>`, dedupes on the event id (delivery is at-least-once),
acts on a command through a `Capability` seam, publishes the output, and
acknowledges what it processed. The `agent-agent` binary runs it with a coding
belt, which reads, searches, patches, and runs commands in the session's
workspace, and a `task` capability, which drives a model and offers that belt as
its tools. The belt is the subject of [The Coding Belt](#the-coding-belt).

## The Shell Capability

The `shell` action's `detail` is `{"argv": [...]}`, and its output reports the
exit code, the captured output, the total bytes written, whether the output was
truncated, whether the timeout fired, and how long it ran.

Three rules make it safe to expose to a model:

- **Output is bounded while it is read.** The pipes are drained on their own
  threads and retained only up to a cap; bytes past it are read and discarded, so
  a command that writes without end neither blocks nor grows the agent's memory.
  Reading it all first is the mistake this replaced, and it OOM-killed the agent.
- **A command that never returns is killed.** The child is waited on under a
  timeout, and the timeout is reported, so a hung command cannot hold the session
  forever.
- **The child runs in the agent's own confinement.** The agent is already inside
  the session's namespace, so a child inherits the policy; the sandbox is the
  boundary, not the per-command spawn.

The command is announced through the loop's `Reporter` before it is spawned, so a
command that runs for minutes is visible while it runs rather than only when it
ends.

## The Task Capability

The `task` action's `detail` is `{"task": "..."}`, and it is where the loops
nest. The agent's own loop turns one command into one result; a task needs many
model turns to get there, so the capability owns the inner loop: ask the model,
run the tools it asks for, feed the results back, and stop when it answers with
no tool call. Its output reports the model's answer and how many turns it took.

The model is a `Model` seam rather than a provider's client, so the whole
capability is exercisable without a network. The first client is an
OpenAI-compatible `chat/completions` endpoint, and it owns the mapping from the
agent's shapes to that wire format. The seam is the agent's own types because a
conversation is four things — a system instruction, the task, what the model
said, and what a tool returned — and a type that names them cannot carry a field
its role has no meaning for.

The capability offers the session's coding belt, which is what makes a task's
tools inherit the session's confinement, its mode, and the bounded output above.
It never spawns a process itself.

**Everything that crosses into the model's context is bounded**, for the reason
the shell capability bounds a command's output: a model's context is a resource
like any other, and a command that writes without end must not be able to fill it.
The shell capability caps a command at a mebibyte, which is right for the log and
far too much for a model, so each stream is cut to 8 KiB before it is fed back —
**separately**, so neither stream can crowd the other out of the model's view —
and the whole result is cut again, so a tool whose detail shape the capability
does not know is bounded too. A cut is marked, so the model knows what it lost. A
task also has a turn bound (32), so a model that never stops calling tools cannot
hold the session, and it reports each turn's prose as progress, bounded in its
turn.

A tool the agent does not offer is reported to the model rather than refused, so
the model is told what happened and gets to correct itself instead of the task
ending.

The provider's credentials are the agent's own and stay in its environment: the
client runs inside the sandbox, so the tunnel stays opaque and TLS stays end to
end. See [sandbox network](../sandbox/network.md).

## The Coding Belt

The agent offers the model a small set of tools over its workspace, and the
**mode** decides which. `read` returns a window of lines from one file.
`code_search` finds a literal substring in the workspace's files. `patch` applies
a unified diff. `shell` runs a program. The binary takes
`--mode readonly|readwrite` as the mode a session starts in, defaulting to
`readwrite`, and a client switches it while the session runs.

A read-only belt offers `read` and `code_search` alone, so nothing it offers can
change the workspace. The mode is the agent's **capability set**, a contract with
the model and with bus clients. It is not a kernel guarantee. The session policy
still binds the workspace read-write, `Policy::validate` requires a writable
workdir, and the policy's mount set is frozen at spawn, so a guaranteed read-only
workspace is a change in `sandbox` and `daemon`. A switch changes what the belt
offers, and it cannot widen what the kernel allows.

The belt holds the mode in one cell, and the model's tool schemas, the bus
dispatch, and the system prompt all derive from it, so a schema cannot name a tool
the belt would refuse and a tool cannot be forgotten in the prompt. An action the
belt does not offer is answered with a reason rather than run, so a client and an
agent of different versions do not crash each other.

Every path a tool is given resolves against the canonical workspace root. An empty
path, an absolute path, a `..` component, a NUL byte, and a symlink that leaves the
workspace are refused, and a write may not name `.git`. Every read of a file is
bounded while it is read. `read` returns at most 2000 lines or 8 KiB of content and
pages with `start_line`; `code_search` bounds its walk by entries, depth, matches,
and text, and reports `truncated` when it stopped early, so a partial search is not
read as the whole workspace.

### The Mode Action

A client switches the mode by publishing an `agent.session.<id>.command` whose
action is `mode`. Its `detail` is absent, `null`, or `{}` to report the mode
without changing it, and `{"mode": "readonly"}` or `{"mode": "readwrite"}` to
switch it. The output names the resulting mode and whether the call changed it, so
a repeated switch is `done` with `changed` false rather than an error.

The model cannot call it. The action is not one of the belt's tools, so it never
appears in the schemas the model is offered, and a model that names it is told the
agent has no tool by that name. Moving a session to `readwrite` is a decision the
client makes.

The loop is synchronous, so a switch lands between commands. A task in flight
finishes under the mode it started with, and the next command, and the next task's
tool list, see the new one. The command is durable, so an agent that restarts
replays it from the log and converges on the same mode.

### The Patch Tool

`patch` takes `{"diff": "..."}`, a unified diff as `git diff` writes one, and
applies it in process. No `git` program and no repository is involved, so a
session's workspace need not be a checkout. The unified diff is only the format
the model and the agent agree on; `diffy` parses it and applies its hunks.

The parse is the syntax check. It refuses a diff whose hunk header does not match
its hunks, a diff that is not a unified diff, and a binary patch, each with a terse
reason the model is told and, where the library adds evidence, the library's own
text beside it. Compiling the patched file would need a toolchain per language and
a policy for files that do not parse, and a session has no toolchain.

Path safety is the belt's own rule. `diffy` does not resolve paths, and no external
program checks them here, so the belt resolves every path the diff names against
the canonical workspace root and refuses an absolute path, a `..` component, a NUL
byte, a symlink that leaves the workspace, and a write under `.git`. A path drops
one leading `a/` or `b/` component if it carries one, and the decision is per path,
so a diff that mixes prefixed and unprefixed headers still applies and
`--- sub/f.txt` resolves to `sub/f.txt`.

`patch` writes nothing until the whole diff is known to apply. It parses the diff,
resolves every path, reads each base file under its cap, and applies every hunk,
holding the result as a plan; only a complete plan is written, so a refusal never
follows a write. A write that fails part-way, from an operating-system error, can
leave the files before it written, and the output reports that.

Every bound is explicit. The diff text is at most 256 KiB, a patch names at most
64 files, one base file is at most 1 MiB, and the final texts together are at most
8 MiB. A base is refused rather than truncated, because half a base would corrupt
the file an applier writes.

## The Client

The first client is an OpenAI-compatible `chat/completions` endpoint, in
`agentd/src/openai/`. It is one implementation of the `Model` seam, so nothing
above it learns what a `role`, a `tool_call`, or a `Bearer` header is: `wire` owns
the mapping and `http` owns the transport.

`wire` maps the agent's four message shapes onto the protocol's and back. One
detail is worth naming: this protocol carries a tool call's arguments as a **JSON
string**, so the mapping serializes them on the way out and parses them back on the
way in. A string that is not JSON is handed on as a string, and the capability
reports it as a malformed command rather than the turn failing, so the model can
correct itself. The reply types accept fields the agent does not read (`id`,
`usage`, ...), because the reply is the provider's document and a provider may add
to it.

`http` is written out rather than delegated, for two reasons the session imposes.
The egress proxy authenticates a `CONNECT` with `Proxy-Authorization: Bearer
<token>`, and no off-the-shelf client sends that header: they turn the proxy URL's
userinfo into `Basic`, which the proxy answers `407`, or they offer no way to set
it. And no CA file is readable inside the sandbox, so the trust store is compiled
into the binary (`webpki-roots`) rather than read from disk. Everything else about
the transport is deliberately small: one request, one reply, `Connection: close`,
HTTP/1.1, and no ALPN.

Every bound is enforced **while reading**: the head at 16 KiB, the body at a
mebibyte, and the whole exchange under one deadline (60 seconds by default). A
reply larger than the cap is refused rather than cut, because a truncated JSON
document is not a reply. This is row 18's rule applied to HTTP, and row 14's
incident is why it is a rule at all.

Only `https` is accepted, for the reason the settings file refuses `http`: the
proxy speaks `CONNECT` alone and the tunnel is opaque, so TLS stays end to end and
a plaintext endpoint has no route at all.

The binary names the endpoint on its command line — `--model`, `--base-url`, and
`--env-key`, which belong together — reads the key's value from the environment
variable `--env-key` names, and finds its proxy in `HTTPS_PROXY`, `ALL_PROXY`, or
`HTTP_PROXY`. Without those flags there is no model and `task` is an unknown
action, which is what a session with no endpoint has. `NO_PROXY` is not read: the
endpoint is never the command's own loopback, and a request that goes to the proxy
when the proxy is reachable is the only route a session has anyway.

The reply is not streamed. The seam answers with a whole turn, the capability
reports each turn's prose as progress, and the deadline bounds one turn; a
streamed reply would need a different seam.

### Secrets

Two values the client holds are credentials: the provider key, which it reads from
the environment variable `--env-key` names, and the proxy's token, which it parses
out of the injected `HTTP_PROXY` URL. Both are held as `secrecy::SecretString`
values rather than `String`s, which buys two properties a plain `String` does not
have: the type cannot be printed (its `Debug` is redacted, so a config, a client,
or a proxy in a log line shows `[REDACTED]`), and the buffer is wiped when the
value drops, so the bytes do not outlive the client in freed memory. The buffers
that carry either secret on the way out are wiped for the same reason: the request
head holds `Authorization: Bearer <key>` and the `CONNECT` head holds
`Proxy-Authorization: Bearer <token>`, and both are written as `Zeroizing<String>`.

What this does not reach is the environment block itself. The value arrives through
`std::env::var`, and the process's environment is a copy the kernel holds; clearing
it takes `std::env::remove_var`, which is `unsafe` in edition 2024 and denied
everywhere in this workspace. The same is true of the child's environment the
daemon builds: the value has to reach the agent's variable, so a copy of it exists
wherever the policy is rendered and in the confined process's own environment, by
design. Zeroizing the copies this process owns is what is in reach, and it is what
is done.

## The Policy Grants the Bus Socket

`sandbox`'s `NetworkPolicy.unix_sockets` is a list of socket paths the command
may connect to. The renderer binds each existing entry into the command's
namespace, read-write, and a `deny` over an entry is a construction error, the
same as the egress socket. A missing entry is a `RenderError::MissingSocket`,
refused rather than skipped, so a session whose bus socket is absent fails before
the command runs. A `deny` over the authority socket's directory is what keeps
the agent out of the authoritative half of the bus. See
[lifecycle.md](./lifecycle.md#authority).

## The Image

An image carries three things.

| Field | Meaning |
| --- | --- |
| `policy` | A builder from a workspace root to a validated `Policy`. |
| `program` | The agent binary and its arguments, resolved on the trusted side. |
| `contract` | The event types the agent publishes and the events it accepts. |

The policy builder is a function, not a stored `Policy`, because a `Policy` names
an absolute workspace root that does not exist until a session is opened. The
builder takes the root, binds it read-write, sets the `workdir` inside it, adds
the egress grant, adds the bus socket, and returns a policy that `Policy::validate`
accepts.

The program and its arguments are resolved with the same trust rule the sandbox
applies to `bwrap` and the supervisor. A binary inside a policy write root is
refused, because a repository must not supply the program that decides what it
does.

## The Settings and the Egress Allowlist

The operator declares the OpenAI-compatible endpoints in a settings file, and the
session's egress allowlist is **derived** from them. One `host:port` rule per
declared endpoint's `base_url`; a host declared by two endpoints is one rule, and
an endpoint on a non-default port is a rule of its own.

```toml
# The model a session uses, and the id sent to the API.
default = "grok-4.7"

[model."grok-4.7"]
base_url = "https://api.x.ai/v1"
env_key = "XAI_API_KEY"
```

The file is `$XDG_CONFIG_HOME/agent/config.toml`, falling back to
`$HOME/.config/agent/config.toml`. It is the only scope: a project-side file
would let a repository grant its own agent egress. A file that is absent declares
no endpoint, and a session with no endpoint reaches nothing — the same
fail-closed default an empty allowlist has. A file that declares endpoints must
name which one a session uses, because the agent is told exactly one model.

Only `https` is accepted. The egress proxy speaks `CONNECT` alone and the sandbox
gives the command no route but that proxy, so a plaintext endpoint cannot be
reached at all; the URL is refused when the file is read rather than at a call
that would look like a network outage. See
[sandbox network](../sandbox/network.md).

The daemon reads the file and the agent never does. Deriving the rule from the same
`base_url` the client calls is what keeps the allowlist and the endpoint from
disagreeing. See [daemon.md](./daemon.md).

## The Image at Launch

An image is fixed before a session exists, so a session's own arguments are added
when the daemon launches it. An image of the project's agent is told three things:
the bus socket it subscribes on (`--socket`), the session id that scopes its input
and output (`--session`), and the workspace its commands run in (`--workdir`). An
image that is not the agent runs exactly the arguments it carries, which is what a
script does; `AgentImage::agent` is what marks the difference, and it is the
settings that build an agent image.

The settings also carry what the agent needs to reach its model: `--model`,
`--base-url`, and `--env-key` in its arguments, and the key's **value** in its
environment, under the name `env_key` gives. The daemon's binary reads that value
from its own environment and the session inherits it, so the agent holds the
credential and the daemon holds no copy of it beyond the launch: the settings file
names the variable and never the value, no report carries it, and the proxy cannot
see it because the tunnel is opaque and TLS is end to end.

One flag of the agent's is **not** injected: `--mode`. The mode belongs to the
deployment's settings rather than to a session's identity, and the daemon's policy
binds the workspace read-write today, so a session starts `readwrite` and a client
may switch it while the session runs.

## The Output Contract

An agent publishes events. The contract names the types, so a consumer reads the
log without guessing and two agents stay comparable.

The agent owns `agent.session.<id>.` and publishes under it. The manager owns
`agent.session.` and publishes the lifecycle under it. The two do not write the
same type.

| Event | Owner | Meaning |
| --- | --- | --- |
| `agent.session.requested` | authority client | A client asks for a session. |
| `agent.session.started` | manager | The agent process is running. |
| `agent.session.stopped` | manager | The agent process was stopped on request. |
| `agent.session.exited` | manager | The agent process ended on its own. |
| `agent.session.failed` | manager | The session could not start. |
| `agent.session.stop_requested` | authority client | A client asks a session to stop. |
| `agent.session.<id>.command` | authority client | Input for the agent, event type `agent.session.command` with the session as `subject`. |
| `agent.session.<id>.output` | agent | The agent's progress and result, event type `agent.session.output`. |

The lifecycle types are gated by authority, because starting and stopping a
session is a privileged act. The agent's own output is not gated, because the
agent is the only writer of it and a forged output event cannot start or stop
anything. The gating list lives in one place, next to the type constants, not
scattered through the handlers.

An output's `kind` says what it is: `started`, `progress`, `done`, or `error`. A
capability reports through the loop's `Reporter` while it works, so an action that
takes minutes is visible while it runs — `shell` announces the argv it is about to
run, and `task` reports what the model said on each turn. A report is **best
effort** and a result is not, which is why a capability cannot fail a command by
reporting: the output the call returns is the result, and it is published over the
same connection, so a dead bus is reported by that publish instead.

## What the Agent Does Not Get

- No credentials. The agent holds its own provider token in its environment, and
  the tunnel is opaque. The sandbox never sees it. See
  [sandbox network](../sandbox/network.md).
- No authority. The agent cannot add an egress rule, approve a request, or stop
  a session. It can open a connection, which becomes a `requested` event.
- No terminal. Its standard input and output are not its interface.
- No memory or CPU ceiling. The kernel does not enforce one, so the policy does
  not claim one.
- No kernel guarantee for its mode. A read-only belt is a contract with the model
  and with clients, and a switch cannot widen what the policy froze at spawn. See
  [The Coding Belt](#the-coding-belt).
