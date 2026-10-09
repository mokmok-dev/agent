import { Context, Deferred, Effect, Layer, Ref, Schema } from "effect";

import { RequestId } from "./access-request.js";
import type {
  AccessRequest,
  DecisionFrame,
  Origin,
  Outcome,
  PendingDecision,
} from "./access-request.js";

interface PendingEntry {
  readonly requestId: RequestId;
  readonly request: AccessRequest;
  readonly origin: Origin;
  readonly deferred: Deferred.Deferred<Outcome>;
}

export class UnknownDecision extends Schema.TaggedError<UnknownDecision>()("UnknownDecision", {
  requestId: Schema.String,
}) {}

export interface ApprovalDesk {
  readonly submit: (request: AccessRequest, origin: Origin) => Effect.Effect<Outcome>;
  readonly pending: () => Effect.Effect<ReadonlyArray<PendingDecision>>;
  readonly decide: (frame: DecisionFrame) => Effect.Effect<void, UnknownDecision>;
}

export const ApprovalDesk: Context.Service<ApprovalDesk, ApprovalDesk> = Context.Service(
  "agent/sandbox-broker/ApprovalDesk",
);

export const ApprovalDeskLayer: Layer.Layer<ApprovalDesk> = Layer.effect(
  ApprovalDesk,
  Effect.gen(function* () {
    const nextId = yield* Ref.make(0);
    const pendingRef = yield* Ref.make<Array<PendingEntry>>([]);

    const toDecision = (entry: PendingEntry): PendingDecision => ({
      requestId: entry.requestId,
      request: entry.request,
      origin: entry.origin,
    });

    const desk: ApprovalDesk = {
      submit: (request, origin) =>
        Effect.acquireUseRelease(
          Effect.gen(function* () {
            const n = yield* Ref.modify(nextId, (i) => [i + 1, i + 1]);
            const requestId = Schema.decodeSync(RequestId)(String(n));
            const deferred = yield* Deferred.make<Outcome>();
            yield* Ref.update(pendingRef, (list) => [
              ...list,
              { requestId, request, origin, deferred },
            ]);
            return { requestId, deferred };
          }),
          ({ deferred }) => Deferred.await(deferred),
          ({ requestId }) =>
            Ref.update(pendingRef, (list) => list.filter((e) => e.requestId !== requestId)),
        ),

      pending: () => Effect.map(Ref.get(pendingRef), (list) => list.map(toDecision)),

      decide: (frame) =>
        Effect.flatMap(
          Ref.modify(pendingRef, (list) => {
            const entry = list.find((e) => e.requestId === frame.requestId);
            return [entry, list.filter((e) => e.requestId !== frame.requestId)] as const;
          }),
          (entry) =>
            entry === undefined
              ? Effect.fail(new UnknownDecision({ requestId: frame.requestId }))
              : Deferred.succeed(entry.deferred, frame.outcome),
        ),
    };

    return desk;
  }),
);
