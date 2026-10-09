# @agent/sandbox-broker

Injects network and filesystem access into a live [Anthropic Sandbox Runtime](https://github.com/anthropics/sandbox-runtime)
(`srt`) session, after a human approves each one. One session is one `Effect` scope: the host states a
base policy once, and every command it wraps is confined to that policy plus whatever has been approved.

The broker exists because `srt` answers "may this connection happen" itself, and a program that embeds
it usually has to ask a person instead. Two things have to be wired for that: `srt`'s ask callback, for
a host a process reaches for and nobody predicted, and this broker's own submit path, for a host the
caller knows it will need.

## When a grant takes effect

The two halves of an `srt` policy behave differently, and the type says which one a grant is in.

| Request                   | Approved                                                                                                                               | Mechanism                                                                                                |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `AllowHost`               | Immediately, including for commands already running                                                                                    | `updateConfig`, whose network lists `srt`'s proxy re-reads on every request                              |
| `AllowRead`, `AllowWrite` | On the next wrap only                                                                                                                  | `customConfig` on `wrapWithSandboxArgv`; `srt` bakes filesystem rules into the `bwrap` argv at wrap time |
| anything denied           | Nothing. The outcome is recorded in the session ledger, so a later ask for the same target is answered `false` without a second prompt |                                                                                                          |

`submit` returns the timing as `granted.effect`, so a caller never has to remember which half it is in.
A `Denied` outcome is a value, not an error: a person saying no is a normal answer.

## Ask a person

The approval channel is its own service, `ApprovalDesk`, so a front end for it depends on approvals
without depending on sandboxes. It has three sides.

`submit` registers a request and suspends the calling fiber until someone decides. Each request gets an
id, and the observe side is a poll over the requests waiting right now:

```ts
const pending = yield * desk.pending();
// [{ requestId: "1", request: { _tag: "AllowHost", pattern: "api.github.com" }, origin: "Predicted" }]
yield * desk.decide({ _tag: "Decision", requestId: "1", outcome: { _tag: "Approved" } });
```

`origin` is `"Predicted"` when the caller asked ahead of running a command, and `"Attempted"` when a
sandboxed process reached for the host mid-connection. It is a label for the person reading the
request; it does not change what the grant does.

Frames travel as JSON through the same `Schema`s, so `encodePendingDecision` and `decodeDecisionFrame`
are the whole wire. Deciding an id that is not waiting fails with `UnknownDecision`.

## The unplanned path

When a wrapped process dials a host that neither `srt`'s allow list nor its deny list decides, `srt`'s
proxy calls the ask callback. The broker turns that into an `AllowHost` request on the same desk, with
`origin: "Attempted"`, and parks the connection until the person answers. An approval is materialized
through the same code path a predicted one uses, so the next connection to that host passes without a
prompt. A denial is remembered for the session.

## Use it as a library

```ts
import { Effect, Layer } from "effect";
import { ApprovalDesk, ApprovalDeskLayer, layerReal, withSession } from "@agent/sandbox-broker";

const base = {
  allowedDomains: [],
  deniedDomains: [],
  denyRead: [".git/config"],
  allowWrite: [],
  denyWrite: [],
};

const program = withSession(
  base,
  (session) =>
    Effect.gen(function* () {
      const desk = yield* ApprovalDesk;
      yield* session.submit({ _tag: "AllowHost", pattern: "api.github.com" });
      const { argv, env } = yield* session.wrap("curl -fsSL https://api.github.com");
      spawn(argv[0], argv.slice(1), { env });
      // The person's front end polls desk.pending() and answers with desk.decide().
    }),
  process.env.SHELL,
).pipe(Effect.provide(Layer.merge(layerReal, ApprovalDeskLayer)));

await Effect.runPromise(program);
```

`withSession` runs `srt`'s `initialize` on acquire and `reset` on release, so a session's lifetime is
its scope. The session value is handed to your `use` and cannot escape it, so there is no
closed-session state to model.

The third argument names the shell `srt` wraps commands with. `srt`'s own default is `/bin/bash`, and a
host without a merged `/usr` does not have one, so name a shell that exists there. The session's
`wrap` returns `{ argv, env }` from `wrapWithSandboxArgv`: spawn it directly, with `env`, and do not
put a shell in front of it.

The broker's `srt` surface is the `SandboxPort` service. `layerReal` is the adapter over
`SandboxManager`; substitute a fake port and no test needs a sandbox.

## Limits worth knowing

- A write grant on Linux binds a concrete path, so the path has to exist on the host when the command
  is wrapped. Granting a directory the sandbox is expected to create does not work.
- On Windows, `srt` `0.0.79` refuses a per-command `allowRead` or `allowWrite`. A wrap after a
  filesystem grant therefore fails there with `PortWrapError` carrying `srt`'s message. The broker
  passes the grant rather than quietly dropping it.
- The ask callback in `0.0.79` returns a plain boolean, so a reason for a denial cannot be handed to
  the sandboxed process. Only the person sees it.
- Only widening requests exist. Narrowing a grant needs a new session, because `srt` cannot undo a
  filesystem rule inside a running one.
- An unanswered request waits until the person answers or the session closes. Closing interrupts every
  parked ask, and `srt` reads the interruption as a denial.

## Develop it

```bash
pnpm install
pnpm typecheck
pnpm test                          # the integration test skips without a sandbox
pnpm exec oxlint apps/sandbox-broker
pnpm mutate
nix flake check
```

`sandbox-runtime.test.ts` drives a real sandbox, and it is where the timing above is measured rather
than asserted from a fake. It skips itself unless `srt` reports a supported platform, reports no
dependency errors, and a shell it can name exists on disk.

So `nix flake check` does not run it. The check derivations carry no sandbox tooling, and a nix build
has no network for the test's wrapped `curl` to reach. Its coverage is local, from the dev shell:

```bash
nix develop -c pnpm test
```

or, outside the shell:

```bash
nix shell nixpkgs#bubblewrap nixpkgs#socat nixpkgs#ripgrep -c pnpm test
```

`repro/` holds the standalone probes that measured the boundary this broker encodes, with their
recorded output in `repro/measured.log`. `repro/run-all.sh` reruns all five.
