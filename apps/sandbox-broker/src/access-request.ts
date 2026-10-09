import { Effect, Schema } from "effect";

export const HostPattern = Schema.NonEmptyString;
export type HostPattern = typeof HostPattern.Type;

export const FsTarget = Schema.Union([
  Schema.String,
  Schema.TaggedStruct("Literal", { path: Schema.String }),
]);
export type FsTarget = typeof FsTarget.Type;

export const AccessRequest = Schema.Union([
  Schema.TaggedStruct("AllowHost", { pattern: HostPattern }),
  Schema.TaggedStruct("AllowRead", { target: FsTarget }),
  Schema.TaggedStruct("AllowWrite", { target: FsTarget }),
]);
export type AccessRequest = typeof AccessRequest.Type;

export const Outcome = Schema.Union([
  Schema.TaggedStruct("Approved", {}),
  Schema.TaggedStruct("Denied", {}),
]);
export type Outcome = typeof Outcome.Type;

export const Origin = Schema.Union([Schema.Literal("Predicted"), Schema.Literal("Attempted")]);
export type Origin = typeof Origin.Type;

export const RequestId = Schema.String.pipe(Schema.brand("RequestId"));
export type RequestId = typeof RequestId.Type;

export const DecisionFrame = Schema.TaggedStruct("Decision", {
  requestId: RequestId,
  outcome: Outcome,
});
export type DecisionFrame = typeof DecisionFrame.Type;

export const PendingDecision = Schema.Struct({
  requestId: RequestId,
  request: AccessRequest,
  origin: Origin,
});
export type PendingDecision = typeof PendingDecision.Type;

export class DecisionFrameError extends Schema.TaggedError<DecisionFrameError>()(
  "DecisionFrameError",
  {
    raw: Schema.String,
    cause: Schema.Unknown,
  },
) {}

export const decodeDecisionFrame = (
  raw: string,
): Effect.Effect<DecisionFrame, DecisionFrameError> =>
  Schema.decodeEffect(Schema.fromJsonString(DecisionFrame))(raw).pipe(
    Effect.mapError((cause) => new DecisionFrameError({ raw, cause })),
  );

export const encodePendingDecision = (decision: PendingDecision): string =>
  Schema.encodeSync(Schema.fromJsonString(PendingDecision))(decision);
