import { Effect, Exit, Fiber, Ref, Scope } from "effect";
import type { NetworkHostPattern } from "@anthropic-ai/sandbox-runtime";

import type { AccessRequest, Origin, Outcome } from "./access-request.js";
import { ApprovalDesk } from "./approval-desk.js";
import {
  type BasePolicy,
  type SessionState,
  type ResolvedGrant,
  type TakeEffect,
  approveInto,
  closeSession,
  initialPolicy,
  ledgerAnswerFor,
  patternOfAsk,
  sessionConfigFor,
  takeEffect,
  wrapConfigFor,
} from "./policy.js";
import { SandboxPort, type WrappedArgv, type PortWrapError } from "./sandbox-port.js";

export interface Granted {
  readonly request: AccessRequest;
  readonly outcome: Outcome;
  readonly effect: TakeEffect;
}

export interface SandboxBroker {
  readonly submit: (request: AccessRequest) => Effect.Effect<Granted>;
  readonly wrap: (command: string) => Effect.Effect<WrappedArgv, PortWrapError>;
}

export const withSession = <A, E, R>(
  base: BasePolicy,
  use: (session: SandboxBroker) => Effect.Effect<A, E, R>,
  binShell?: string,
): Effect.Effect<
  A,
  E | import("./sandbox-port.js").PortInitializeError,
  R | SandboxPort | ApprovalDesk
> =>
  Effect.gen(function* () {
    const port = yield* SandboxPort;
    const desk = yield* ApprovalDesk;
    const state = yield* Ref.make<SessionState>({
      policy: initialPolicy(base),
      resolved: [],
      open: true,
    });
    const bridgeScope = yield* Scope.make();

    const materialize = (request: AccessRequest, outcome: Outcome): Effect.Effect<void> =>
      Ref.modify(state, (s) => {
        const resolved: ReadonlyArray<ResolvedGrant> = [...s.resolved, { request, outcome }];
        const approved = s.open && outcome._tag === "Approved";
        const policy = approved ? approveInto(s.policy, request) : s.policy;
        const config =
          approved && request._tag === "AllowHost" ? sessionConfigFor(policy) : undefined;
        return [config, { policy, resolved, open: s.open }] as const;
      }).pipe(
        Effect.flatMap((config) =>
          config === undefined ? Effect.void : Effect.sync(() => port.updateConfig(config)),
        ),
      );

    const submit = (request: AccessRequest, origin: Origin): Effect.Effect<Granted> =>
      Effect.gen(function* () {
        const outcome = yield* desk.submit(request, origin);
        yield* materialize(request, outcome);
        return { request, outcome, effect: takeEffect(request, outcome) };
      });

    const sessionAnswer: (ask: NetworkHostPattern) => Effect.Effect<boolean> = (ask) =>
      Effect.flatMap(Ref.get(state), (s) => {
        const ledger = ledgerAnswerFor(s.resolved, ask);
        if (ledger !== undefined) {
          return Effect.succeed(ledger);
        }
        return Effect.map(
          submit({ _tag: "AllowHost", pattern: patternOfAsk(ask) }, "Attempted"),
          (granted) => granted.outcome._tag === "Approved",
        );
      });

    const ask = (params: NetworkHostPattern): Promise<boolean> =>
      Effect.runPromise(
        Effect.forkIn(bridgeScope)(sessionAnswer(params)).pipe(
          Effect.flatMap(Fiber.join),
          Effect.orElseSucceed(() => false),
        ),
      );

    const session: SandboxBroker = {
      submit: (request) => submit(request, "Predicted"),
      wrap: (command) =>
        Effect.gen(function* () {
          const s = yield* Ref.get(state);
          return yield* port.wrapArgv(command, wrapConfigFor(s.policy), binShell);
        }),
    };

    return yield* Effect.acquireUseRelease(
      port.initialize(sessionConfigFor(initialPolicy(base)), ask),
      () => use(session),
      () =>
        Effect.gen(function* () {
          yield* Ref.update(state, closeSession);
          yield* Scope.close(bridgeScope, Exit.void);
          yield* port.reset();
        }),
    );
  });
