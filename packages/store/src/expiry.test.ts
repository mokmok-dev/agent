import { describe, expect, test } from "bun:test";
import type { Session, Turn } from "./session.ts";
import {
  ABANDONED_TURN_REASON,
  expireTurn,
  GATEWAY_CLAIM_DEADLINE_MS,
  hasExpired,
  nextDeadline,
  NO_GATEWAY_REASON,
  NO_MODEL_REASON,
  offersModel,
  TURN_ABANDONED_DEADLINE_MS,
} from "./session.ts";

/**
 * A `stdio` turn waits for the gateway that hosts its agent. These tests pin
 * what happens when no gateway ever arrives, so a stopped VM cannot leave a
 * thread silent.
 */

const session = (overrides: Partial<Session> = {}): Session =>
  ({
    id: "s1",
    thread: { provider: "linear", workspaceId: "w", channelId: "i", threadId: "t" },
    status: "active",
    acpSessionId: null,
    gatewayId: null,
    selection: {
      agent: "claude-code",
      runtime: "local-vm",
      protocol: "stdio",
      model: { provider: "anthropic", modelId: "m" },
    },
    conversationId: "c1",
    turnSeq: 1,
    createdAt: 0,
    updatedAt: 0,
    ...overrides,
  }) as Session;

const turn = (overrides: Partial<Turn> = {}): Turn =>
  ({
    id: "t1",
    sessionId: "s1",
    prompt: "hello",
    status: "queued",
    idempotencyKey: "evt-1",
    stopReason: null,
    error: null,
    createdAt: 0,
    updatedAt: 0,
    ...overrides,
  }) as Turn;

describe("the claim deadline", () => {
  test("a queued turn is live before its deadline", () => {
    expect(hasExpired(turn(), GATEWAY_CLAIM_DEADLINE_MS - 1)).toBe(false);
  });

  test("a queued turn expires at its deadline, not after it", () => {
    expect(hasExpired(turn(), GATEWAY_CLAIM_DEADLINE_MS)).toBe(true);
  });

  test("the deadline is five minutes", () => {
    expect(GATEWAY_CLAIM_DEADLINE_MS).toBe(300_000);
  });

  test("a claimed turn outlives the claim deadline, which only governs a queued one", () => {
    // The claim deadline governs only a turn no gateway took; a claimed turn is
    // judged against the longer abandonment grace instead.
    for (const status of ["dispatched", "running"] as const) {
      const fresh = turn({ status, createdAt: 0, updatedAt: 0 });
      expect(hasExpired(fresh, GATEWAY_CLAIM_DEADLINE_MS + 1)).toBe(false);
      expect(hasExpired(turn({ status: "queued" }), GATEWAY_CLAIM_DEADLINE_MS + 1)).toBe(true);
    }
  });

  test("a claimed turn expires once its gateway has been silent past the grace", () => {
    for (const status of ["dispatched", "running"] as const) {
      const silent = turn({ status, createdAt: 0, updatedAt: 0 });
      expect(hasExpired(silent, TURN_ABANDONED_DEADLINE_MS)).toBe(true);
      expect(hasExpired(silent, TURN_ABANDONED_DEADLINE_MS - 1)).toBe(false);
    }
  });

  test("a settled turn never expires", () => {
    for (const status of ["completed", "failed", "cancelled"] as const) {
      const settled = turn({ status, createdAt: 0, updatedAt: 0 });
      expect(hasExpired(settled, GATEWAY_CLAIM_DEADLINE_MS * 10)).toBe(false);
      expect(hasExpired(settled, TURN_ABANDONED_DEADLINE_MS * 10)).toBe(false);
    }
  });

  test("a caller may state its own deadline, which is what a test does", () => {
    expect(hasExpired(turn(), 100, 100)).toBe(true);
    expect(hasExpired(turn(), 99, 100)).toBe(false);
  });

  test("the wait is the time since the turn was created, not the sum of the two times", () => {
    // A turn created at 900 with the clock at 1_000 has waited 100, which is
    // inside a 500 deadline; adding the two times would reach 1_900 and call it
    // expired.
    expect(hasExpired(turn({ createdAt: 900 }), 1_000, 500)).toBe(false);
    // The same turn does expire once the wait itself passes the deadline.
    expect(hasExpired(turn({ createdAt: 400 }), 1_000, 500)).toBe(true);
  });
});

describe("an abandoned turn's reason", () => {
  test("it names the gateway that went quiet, not the one that never arrived", () => {
    // These are different faults and the thread has to be able to tell them
    // apart: one is a missing runtime, the other a run that started and died.
    expect(ABANDONED_TURN_REASON).toContain("claimed this turn");
    expect(ABANDONED_TURN_REASON).toContain("stopped reporting");
    expect(ABANDONED_TURN_REASON).not.toBe(NO_GATEWAY_REASON);
  });

  test("the grace is measured from when the gateway last reported, not from creation", () => {
    // A turn claimed long after it was created must not be failed on arrival.
    const claimedLate = turn({ status: "running", createdAt: 0, updatedAt: 500_000 });
    expect(hasExpired(claimedLate, 500_000 + TURN_ABANDONED_DEADLINE_MS - 1)).toBe(false);
    expect(hasExpired(claimedLate, 500_000 + TURN_ABANDONED_DEADLINE_MS)).toBe(true);
  });

  test("a refused transition yields no turn, which is what a closed session gives", () => {
    // A closed session refuses every move, so the sweep must report nothing
    // rather than the failed turn it could not store.
    const abandoned = turn({ status: "running", createdAt: 0, updatedAt: 0 });
    expect(
      expireTurn(abandoned, session({ status: "closed" }), TURN_ABANDONED_DEADLINE_MS),
    ).toBeUndefined();
  });
});

describe("expiring a turn", () => {
  test("an unclaimed turn fails with a reason that names the missing gateway", () => {
    const expired = expireTurn(turn(), session(), GATEWAY_CLAIM_DEADLINE_MS);
    expect(expired).toMatchObject({ status: "failed", error: NO_GATEWAY_REASON });
    expect(expired?.error).toContain("no gateway claimed this turn");
    expect(expired?.error).toContain("was not connected");
  });

  test("a turn inside its deadline is left alone", () => {
    expect(expireTurn(turn(), session(), GATEWAY_CLAIM_DEADLINE_MS - 1)).toBeUndefined();
  });

  test("a claimed turn is left alone while its gateway is still reporting", () => {
    // The claim deadline only governs a turn no gateway took. A claimed turn is
    // judged against the longer abandonment grace instead.
    const claimed = turn({ status: "dispatched", createdAt: 0, updatedAt: 0 });
    expect(expireTurn(claimed, session(), GATEWAY_CLAIM_DEADLINE_MS)).toBeUndefined();
  });

  test("a claimed turn whose gateway stopped reporting is failed as abandoned", () => {
    // A gateway that dies mid-turn leaves the turn running for ever, and a
    // gateway asking for the next session then skips that thread altogether.
    const abandoned = turn({ status: "running", createdAt: 0, updatedAt: 0 });
    expect(expireTurn(abandoned, session(), TURN_ABANDONED_DEADLINE_MS)).toMatchObject({
      status: "failed",
      error: ABANDONED_TURN_REASON,
    });
    // A turn still inside the grace is left alone.
    expect(expireTurn(abandoned, session(), TURN_ABANDONED_DEADLINE_MS - 1)).toBeUndefined();
  });

  test("a turn in a closed session is not failed by the sweep", () => {
    // Closing is the operator's decision; rewriting its turns afterwards would
    // report a gateway that was never owed a claim.
    expect(
      expireTurn(turn(), session({ status: "closed" }), GATEWAY_CLAIM_DEADLINE_MS),
    ).toBeUndefined();
  });

  test("expiring twice is the same as expiring once, because the second sweep sees a terminal turn", () => {
    const once = expireTurn(turn(), session(), GATEWAY_CLAIM_DEADLINE_MS);
    expect(once).toBeDefined();
    expect(expireTurn(once!, session(), GATEWAY_CLAIM_DEADLINE_MS * 2)).toBeUndefined();
  });

  test("the expired turn records when it failed, so the record shows the wait", () => {
    const expired = expireTurn(
      turn({ createdAt: 1_000 }),
      session(),
      1_000 + GATEWAY_CLAIM_DEADLINE_MS,
    );
    expect(expired?.updatedAt).toBe(1_000 + GATEWAY_CLAIM_DEADLINE_MS);
  });
});

describe("when a session next owes a decision", () => {
  test("a queued turn is due at its own deadline", () => {
    // The turn was created at 0, so its deadline is the deadline itself. A host
    // that wakes then finds it expired, which is what makes the wait observable.
    expect(nextDeadline([turn()], 0)).toBe(GATEWAY_CLAIM_DEADLINE_MS);
  });

  test("the soonest of several queued turns is the one that matters", () => {
    const turns = [
      turn({ id: "late", createdAt: 1_000 }),
      turn({ id: "early", createdAt: 100, idempotencyKey: "evt-2" }),
    ];
    // The later turn alone would schedule a wake-up after the earlier one's
    // deadline, leaving the earlier turn waiting past its own failure.
    expect(nextDeadline(turns, 0)).toBe(100 + GATEWAY_CLAIM_DEADLINE_MS);
  });

  test("two turns with the same deadline still produce that deadline", () => {
    // A tie is not a reason to schedule nothing: both turns expire at that
    // instant, so a host that skipped it would leave both waiting.
    const turns = [turn({ id: "a" }), turn({ id: "b", idempotencyKey: "evt-2" })];
    expect(nextDeadline(turns, 0)).toBe(GATEWAY_CLAIM_DEADLINE_MS);
  });

  test("a later turn does not replace an earlier deadline it was compared against", () => {
    // Ordered so the second turn is the later one: if the comparison let it
    // win, the first turn would wait past its deadline to be failed.
    const turns = [turn({ id: "early", createdAt: 0 }), turn({ id: "late", createdAt: 9_000 })];
    expect(nextDeadline(turns, 0)).toBe(GATEWAY_CLAIM_DEADLINE_MS);
  });

  test("a claimed turn has no deadline, because its gateway is answering", () => {
    for (const status of ["dispatched", "running"] as const) {
      expect(nextDeadline([turn({ status })], 0)).toBeUndefined();
    }
  });

  test("a settled turn has no deadline", () => {
    for (const status of ["completed", "failed", "cancelled"] as const) {
      expect(nextDeadline([turn({ status })], 0)).toBeUndefined();
    }
  });

  test("an empty session has nothing to schedule", () => {
    expect(nextDeadline([], 0)).toBeUndefined();
  });

  test("a deadline already past is due now, so a late sweep still sweeps", () => {
    // A host that arms an alarm for a time in the past would wait for nothing;
    // the answer is the current instant, which fires at once.
    expect(nextDeadline([turn({ createdAt: 0 })], 10 * GATEWAY_CLAIM_DEADLINE_MS)).toBe(
      10 * GATEWAY_CLAIM_DEADLINE_MS,
    );
  });

  test("the deadline is when the turn was created plus the wait, not the clock plus it", () => {
    // A turn created at 900, with the clock at 1_000, is due at 900 + the
    // deadline. Measuring from the clock instead would push it a whole wait out.
    expect(nextDeadline([turn({ createdAt: 900 })], 1_000, 500)).toBe(1_400);
  });

  test("a caller may state its own deadline, which is what a test does", () => {
    expect(nextDeadline([turn()], 0, 100)).toBe(100);
  });
});

describe("a model the agent does not offer", () => {
  test("the reason names the model and every model the agent did list", () => {
    const reason = NO_MODEL_REASON(
      { availableModels: ["grok-4.6", "grok-4.7"], statesModels: true },
      "claude-opus-4-7",
    );
    expect(reason).toContain("claude-opus-4-7");
    expect(reason).toContain("grok-4.6, grok-4.7");
  });

  test("an agent that listed nothing is described as listing no models", () => {
    const reason = NO_MODEL_REASON({ availableModels: [], statesModels: false }, "m");
    expect(reason).toContain("no models");
    // A made-up list would read as if the agent offered something.
    expect(reason).not.toContain(",");
  });

  test("a model the agent listed is offered, and one it did not is not", () => {
    const capability = { availableModels: ["a"], statesModels: true };
    expect(offersModel(capability, "a")).toBe(true);
    expect(offersModel(capability, "b")).toBe(false);
  });

  test("an agent that named no models offers none, whatever list it carries", () => {
    expect(offersModel({ availableModels: ["a"], statesModels: false }, "a")).toBe(false);
  });
});
