import { Effect, Schema } from "effect";
import { describe, expect, it } from "vitest";
import * as fc from "fast-check";

import {
  AccessRequest,
  type DecisionFrame,
  FsTarget,
  Origin,
  Outcome,
  PendingDecision,
  RequestId,
  decodeDecisionFrame,
  encodePendingDecision,
} from "./access-request.js";

const arbPendingDecision = (): fc.Arbitrary<typeof PendingDecision.Type> =>
  fc.record({
    requestId: fc.string({ minLength: 1 }),
    request: fc.oneof(
      fc.record({ _tag: fc.constant("AllowHost"), pattern: fc.string({ minLength: 1 }) }),
      fc.record({
        _tag: fc.constant("AllowRead"),
        target: fc.oneof(
          fc.string(),
          fc.record({ _tag: fc.constant("Literal"), path: fc.string() }),
        ),
      }),
      fc.record({
        _tag: fc.constant("AllowWrite"),
        target: fc.oneof(
          fc.string(),
          fc.record({ _tag: fc.constant("Literal"), path: fc.string() }),
        ),
      }),
    ),
    origin: fc.oneof(fc.constant("Predicted"), fc.constant("Attempted")),
  }) as fc.Arbitrary<typeof PendingDecision.Type>;

describe("access-request", () => {
  it("encodes a pending decision to JSON and back", () => {
    const decision: typeof PendingDecision.Type = {
      requestId: "1" as typeof RequestId.Type,
      request: { _tag: "AllowHost", pattern: "api.github.com" },
      origin: "Predicted",
    };
    const raw = encodePendingDecision(decision);
    const parsed = JSON.parse(raw);
    expect(parsed).toEqual({
      requestId: "1",
      request: { _tag: "AllowHost", pattern: "api.github.com" },
      origin: "Predicted",
    });
  });

  it("round-trips a decision frame through decode", async () => {
    const frame: DecisionFrame = {
      _tag: "Decision",
      requestId: "7" as typeof RequestId.Type,
      outcome: { _tag: "Approved" },
    };
    const result = await Effect.runPromise(decodeDecisionFrame(JSON.stringify(frame)));
    expect(result).toEqual(frame);
  });

  it("rejects a malformed frame with DecisionFrameError carrying raw and cause", async () => {
    const result = await Effect.runPromise(Effect.result(decodeDecisionFrame("not json")));
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") {
      const error = result.failure;
      expect(error._tag).toBe("DecisionFrameError");
      expect(error.raw).toBe("not json");
      expect(error.cause).toBeDefined();
    }
  });

  it("decodes every pending decision shape", async () => {
    await fc.assert(
      fc.asyncProperty(arbPendingDecision(), async (decision) => {
        const raw = encodePendingDecision(decision);
        const parsed = JSON.parse(raw);
        expect(parsed.requestId).toBe(decision.requestId);
        expect(parsed.origin).toBe(decision.origin);
        expect(parsed.request._tag).toBe(decision.request._tag);
      }),
    );
  });

  it("rejects a garbage string with DecisionFrameError", async () => {
    await fc.assert(
      fc.asyncProperty(fc.string(), async (raw) => {
        if (raw === "") return;
        const result = await Effect.runPromise(Effect.result(decodeDecisionFrame(raw)));
        if (result._tag === "Failure") {
          expect(result.failure._tag).toBe("DecisionFrameError");
        }
      }),
    );
  });

  it("brands RequestId so the identifier is not a plain string", () => {
    const id = Schema.decodeSync(RequestId)("abc");
    expect(id).toBe("abc");
  });

  it("accepts exactly the two origin literals", () => {
    expect(Schema.decodeSync(Origin)("Predicted")).toBe("Predicted");
    expect(Schema.decodeSync(Origin)("Attempted")).toBe("Attempted");
    expect(() => Schema.decodeUnknownSync(Origin)("Other")).toThrow();
  });

  it("brands RequestId with the identifier RequestId", () => {
    expect(RequestId.identifier).toBe("RequestId");
  });

  it("rejects an outcome tag other than Approved or Denied", () => {
    expect(() => Schema.decodeUnknownSync(Outcome)({ _tag: "" })).toThrow();
    expect(Schema.decodeUnknownSync(Outcome)({ _tag: "Approved" })).toEqual({
      _tag: "Approved",
    });
    expect(Schema.decodeUnknownSync(Outcome)({ _tag: "Denied" })).toEqual({
      _tag: "Denied",
    });
  });

  it("rejects an AllowRead request without a target", () => {
    expect(() => Schema.decodeUnknownSync(AccessRequest)({ _tag: "AllowRead" })).toThrow();
  });

  it("rejects an AllowWrite request without a target", () => {
    expect(() => Schema.decodeUnknownSync(AccessRequest)({ _tag: "AllowWrite" })).toThrow();
  });

  it("rejects a Literal filesystem target without a path", () => {
    expect(() => Schema.decodeUnknownSync(FsTarget)({ _tag: "Literal" })).toThrow();
  });

  it("rejects a pending decision missing any of its fields", () => {
    expect(() => Schema.decodeUnknownSync(PendingDecision)({})).toThrow();
    expect(() => Schema.decodeUnknownSync(PendingDecision)({ requestId: "1" })).toThrow();
  });
});
