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
acknowledges what it processed. The `agent-agent` binary runs it with an `echo`
capability, which is the seam the coding agent replaces in milestone 6.

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

## What the Agent Does Not Get

- No credentials. The agent holds its own provider token in its environment, and
  the tunnel is opaque. The sandbox never sees it. See
  [sandbox network](../sandbox/network.md).
- No authority. The agent cannot add an egress rule, approve a request, or stop
  a session. It can open a connection, which becomes a `requested` event.
- No terminal. Its standard input and output are not its interface.
- No memory or CPU ceiling. The kernel does not enforce one, so the policy does
  not claim one.
