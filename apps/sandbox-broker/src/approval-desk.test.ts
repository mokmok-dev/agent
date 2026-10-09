import { Effect, Fiber } from "effect";
import { describe, expect, it } from "vitest";

import type { AccessRequest, RequestId } from "./access-request.js";
import { ApprovalDesk, ApprovalDeskLayer } from "./approval-desk.js";

function runWithDesk<A, E>(
  run: (desk: ApprovalDesk) => Effect.Effect<A, E, ApprovalDesk>,
): Promise<A> {
  return Effect.runPromise(
    Effect.gen(function* () {
      const desk = yield* ApprovalDesk;
      return yield* run(desk);
    }).pipe(Effect.provide(ApprovalDeskLayer)),
  );
}

describe("approval-desk", () => {
  it("uses the expected Context tag key", () => {
    expect(ApprovalDesk.key).toBe("agent/sandbox-broker/ApprovalDesk");
  });

  it("assigns ids 1 and 2 in submission order", async () => {
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x" };
    await runWithDesk((desk) =>
      Effect.gen(function* () {
        const fiber1 = yield* Effect.forkDetach(desk.submit(request, "Predicted"), {
          startImmediately: true,
        });
        const fiber2 = yield* Effect.forkDetach(desk.submit(request, "Predicted"), {
          startImmediately: true,
        });
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        yield* desk.decide({
          _tag: "Decision",
          requestId: "2" as RequestId,
          outcome: { _tag: "Denied" },
        });
        const r1 = yield* Fiber.join(fiber1);
        const r2 = yield* Fiber.join(fiber2);
        expect(r1).toEqual({ _tag: "Approved" });
        expect(r2).toEqual({ _tag: "Denied" });
      }),
    );
  });

  it("does not resume a submit until a decision arrives", async () => {
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x" };
    await runWithDesk((desk) =>
      Effect.gen(function* () {
        const fiber = yield* Effect.forkDetach(desk.submit(request, "Predicted"), {
          startImmediately: true,
        });
        yield* Effect.sleep("5 millis");
        const pending = yield* desk.pending();
        expect(pending).toEqual([{ requestId: "1", request, origin: "Predicted" }]);
        yield* Fiber.interrupt(fiber);
      }),
    );
  });

  it("removes an entry after deciding and rejects a second decision on the same id", async () => {
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x" };
    await runWithDesk((desk) =>
      Effect.gen(function* () {
        const fiber = yield* Effect.forkDetach(desk.submit(request, "Predicted"), {
          startImmediately: true,
        });
        yield* desk.decide({
          _tag: "Decision",
          requestId: "1" as RequestId,
          outcome: { _tag: "Approved" },
        });
        const outcome = yield* Fiber.join(fiber);
        expect(outcome).toEqual({ _tag: "Approved" });
        expect(yield* desk.pending()).toEqual([]);
        const second = yield* Effect.result(
          desk.decide({
            _tag: "Decision",
            requestId: "1" as RequestId,
            outcome: { _tag: "Approved" },
          }),
        );
        expect(second._tag).toBe("Failure");
        if (second._tag === "Failure") {
          expect(second.failure._tag).toBe("UnknownDecision");
          expect(second.failure.requestId).toBe("1");
        }
      }),
    );
  });

  it("removes a pending entry when the submitting fiber is interrupted", async () => {
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x" };
    await runWithDesk((desk) =>
      Effect.scoped(
        Effect.gen(function* () {
          const fiber = yield* Effect.forkScoped(desk.submit(request, "Predicted"), {
            startImmediately: true,
          });
          yield* Effect.sleep("5 millis");
          yield* Fiber.interrupt(fiber);
          const pending = yield* desk.pending();
          expect(pending).toEqual([]);
        }),
      ),
    );
  });

  it("keeps pending entries in submission order", async () => {
    const r1: AccessRequest = { _tag: "AllowHost", pattern: "a" };
    const r2: AccessRequest = { _tag: "AllowHost", pattern: "b" };
    const r3: AccessRequest = { _tag: "AllowRead", target: "/c" };
    await runWithDesk((desk) =>
      Effect.gen(function* () {
        yield* Effect.forkDetach(desk.submit(r1, "Predicted"), { startImmediately: true });
        yield* Effect.forkDetach(desk.submit(r2, "Attempted"), { startImmediately: true });
        yield* Effect.forkDetach(desk.submit(r3, "Predicted"), { startImmediately: true });
        const pending = yield* desk.pending();
        expect(pending.map((d) => d.requestId)).toEqual(["1", "2", "3"]);
        expect(pending.map((d) => d.origin)).toEqual(["Predicted", "Attempted", "Predicted"]);
      }),
    );
  });

  it("decodes a frame for an unknown id even though decide will fail", async () => {
    const raw = JSON.stringify({
      _tag: "Decision",
      requestId: "ghost",
      outcome: { _tag: "Approved" },
    });
    const { decodeDecisionFrame } = await import("./access-request.js");
    await runWithDesk((desk) =>
      Effect.gen(function* () {
        const decoded = yield* decodeDecisionFrame(raw);
        expect(decoded.requestId).toBe("ghost");
        const result = yield* Effect.result(desk.decide(decoded));
        expect(result._tag).toBe("Failure");
      }),
    );
  });

  it("rejects a decision for a different pending id and leaves the pending list intact", async () => {
    const request: AccessRequest = { _tag: "AllowHost", pattern: "x" };
    await runWithDesk((desk) =>
      Effect.scoped(
        Effect.gen(function* () {
          const fiber = yield* Effect.forkScoped(desk.submit(request, "Predicted"), {
            startImmediately: true,
          });
          yield* Effect.sleep("5 millis");
          const pendingBefore = yield* desk.pending();
          expect(pendingBefore.map((d) => d.requestId)).toEqual(["1"]);
          const result = yield* Effect.result(
            desk.decide({
              _tag: "Decision",
              requestId: "2" as RequestId,
              outcome: { _tag: "Approved" },
            }),
          );
          expect(result._tag).toBe("Failure");
          if (result._tag === "Failure") {
            expect(result.failure._tag).toBe("UnknownDecision");
            expect(result.failure.requestId).toBe("2");
          }
          const pendingAfter = yield* desk.pending();
          expect(pendingAfter.map((d) => d.requestId)).toEqual(["1"]);
          yield* Fiber.interrupt(fiber);
        }),
      ),
    );
  });
});
