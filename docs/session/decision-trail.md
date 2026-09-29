# Decision trail: session-manager milestones 3 to 6

One row per decision, in order. Status is `done`, `open`, or `blocked`.
Evidence is a file, a command, a SHA, or a URL.

| # | Decision | Why | Evidence | Result |
| --- | --- | --- | --- | --- |
| 1 | Land milestone 2 | PR 42 was green and merge-ready; the operator stepped away and asked for all jobs finished | `gh pr view 42`; squash `1ab6484` | done |
| 2 | Do milestones 3 and 4 as two PRs, not one | One PR would land code no test drives. Split at the design's own boundary: the bus client is verifiable against a real bus server alone; the start sequence then composes it with the sandbox | `docs/session/daemon.md` | done |
| 3 | Fix the authority gap first, as its own PR | Found while grounding milestone 3: `requires_authority` did not gate `agent.session.requested` / `stop_requested`, though `docs/session/lifecycle.md` says they are privileged. A security fix ships alone and first | PR 43, commit `1877a58` | done |
| 4 | Bus client: a synchronous facade over an async connection on one thread | The session core and the egress `Approver::ask` are blocking; the transport wants one connection that both reads and writes. tokio is the client's private detail | `daemon/src/bus.rs`; `daemon/tests/bus_client.rs` (6 tests, real server) | done |
| 5 | One connection, the ordinary listener, for milestone 3 | The lifecycle events and `egress.requested` are ungated; only the decisions and rule changes need the authority connection, and those are a later milestone | PR 43; `agent/src/authority.rs` | done |
| 6 | Milestone 4 splits again: image + start sequence + teardown first, then the real launcher and egress serve loop | The real launcher needs bubblewrap, which CI cannot spawn. A fake launcher proves the start sequence, the fail-closed gate, the lifecycle events, and the teardown on any host; the real composition needs a namespace-capable host and lands separately | `daemon/src/manager.rs`; `daemon/tests/manager.rs` (8 tests) | done |
| 7 | The real `SandboxLauncher` composition and the egress proxy serve loop | Needs a host that can spawn a namespace; will be verified on this dev host and skip on CI | TBD | open |
| 8 | The agent crate (milestone 5) landed as `agentd`, with the bus client moved to `agent::client` first | The confined agent needs the same connection logic the daemon has, and a protocol client belongs with the protocol; `daemon::bus` keeps only the sandbox `Publisher` bridge and re-exports the rest | `agent/src/client.rs`, `daemon/src/bus.rs` | done |
| 9 | The agent's capability is a `Capability` trait with an `echo` implementation | The design says milestone 5 is the loop and contract, and the coding agent's real capability is milestone 6's decision; the seam keeps this PR testable end to end without it | `agentd/src/loopcore.rs` | done |
| 10 | M6 blocked on the operator: which capability, which provider, how autonomous | The design does not say what the agent does with a command; deciding it unilaterally would be a product call | TBD | open |
