import { describe, expect, test } from "bun:test";
import type { Session, Turn } from "./session.ts";
import {
  expireTurn,
  GATEWAY_CLAIM_DEADLINE_MS,
  hasExpired,
  NO_GATEWAY_REASON,
  NO_MODEL_REASON,
  offersModel,
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

  test("a claimed turn never expires, because its gateway is answering", () => {
    for (const status of ["dispatched", "running"] as const) {
      expect(hasExpired(turn({ status }), GATEWAY_CLAIM_DEADLINE_MS * 10)).toBe(false);
    }
  });

  test("a settled turn never expires", () => {
    for (const status of ["completed", "failed", "cancelled"] as const) {
      expect(hasExpired(turn({ status }), GATEWAY_CLAIM_DEADLINE_MS * 10)).toBe(false);
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

  test("a claimed turn is left alone, so a slow agent is not failed", () => {
    const claimed = turn({ status: "dispatched" });
    expect(expireTurn(claimed, session(), GATEWAY_CLAIM_DEADLINE_MS * 10)).toBeUndefined();
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
