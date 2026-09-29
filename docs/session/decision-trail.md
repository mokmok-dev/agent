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
| 8 | The agent crate (milestone 5) | The purpose-built agent that subscribes to the bus; milestone 6's coding-agent image builds on it | TBD | open |
