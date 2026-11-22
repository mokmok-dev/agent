import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  acceptsTurns,
  isTerminal,
  nextQueued,
  TERMINAL_TURN_STATUSES,
  threadKey,
  transitionTurn,
  TURN_TRANSITIONS,
  type Session,
  type Turn,
  type TurnStatus,
  type TurnUpdate,
} from "./session.ts";

const session = (overrides: Partial<Session> = {}): Session => ({
  id: "s1",
  thread: { provider: "slack", workspaceId: "T1", channelId: "C1", threadId: "1.5" },
  status: "active",
  acpSessionId: null,
  gatewayId: null,
  selection: null,
  conversationId: null,
  turnSeq: 0,
  createdAt: 0,
  updatedAt: 0,
  ...overrides,
});

const turn = (overrides: Partial<Turn> = {}): Turn => ({
  id: "t1",
  sessionId: "s1",
  prompt: "hello",
  status: "queued",
  idempotencyKey: "evt-1",
  stopReason: null,
  error: null,
  createdAt: 1,
  updatedAt: 1,
  ...overrides,
});

describe("threadKey", () => {
  test("is the provider and the three ids joined by colons", () => {
    expect(
      threadKey({ provider: "slack", workspaceId: "T1", channelId: "C1", threadId: "1.5" }),
    ).toBe("slack:T1:C1:1.5");
    expect(threadKey({ provider: "discord", workspaceId: "G", channelId: "C", threadId: "" })).toBe(
      "discord:G:C:",
    );
    expect(
      threadKey({ provider: "linear", workspaceId: "org", channelId: "issue", threadId: "x" }),
    ).toBe("linear:org:issue:x");
  });

  test("different threads never share a key", () => {
    fc.assert(
      fc.property(
        fc.constantFrom("slack", "discord", "linear"),
        fc.string(),
        fc.string(),
        fc.string(),
        fc.constantFrom("slack", "discord", "linear"),
        fc.string(),
        fc.string(),
        fc.string(),
        (p1, w1, c1, t1, p2, w2, c2, t2) => {
          const a = threadKey({ provider: p1, workspaceId: w1, channelId: c1, threadId: t1 });
          const b = threadKey({ provider: p2, workspaceId: w2, channelId: c2, threadId: t2 });
          const same = p1 === p2 && w1 === w2 && c1 === c2 && t1 === t2;
          expect(a === b).toBe(same);
        },
      ),
    );
  });
});

describe("the transition table", () => {
  test("declares every status", () => {
    expect(Object.keys(TURN_TRANSITIONS).sort()).toEqual([
      "cancelled",
      "completed",
      "dispatched",
      "failed",
      "queued",
      "running",
    ]);
  });

  test("a dispatched turn can fail before it runs, because a dispatch itself can fail", () => {
    const result = transitionTurn(
      turn({ status: "dispatched" }),
      session(),
      { status: "failed", error: "no key" },
      60,
    );
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({ status: "failed", error: "no key" }),
    });
  });

  test("a queued turn can fail without a dispatch, which is what an unclaimed gateway turn does", () => {
    // A `stdio` turn waits for the gateway that hosts its agent. When none
    // claims it, the turn fails from `queued`; see `expireTurn`.
    const result = transitionTurn(turn(), session(), { status: "failed", error: "no gateway" }, 60);
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({ status: "failed", error: "no gateway" }),
    });
  });

  test("the terminal statuses are the ones with no outgoing edge", () => {
    const computed = (Object.keys(TURN_TRANSITIONS) as TurnStatus[]).filter(
      (status) => TURN_TRANSITIONS[status].length === 0,
    );
    expect(computed.sort()).toEqual([...TERMINAL_TURN_STATUSES].sort());
  });

  test("isTerminal agrees with the table", () => {
    for (const status of Object.keys(TURN_TRANSITIONS) as TurnStatus[]) {
      expect(isTerminal(status)).toBe(TURN_TRANSITIONS[status].length === 0);
    }
  });

  test("a finished turn is reported terminal", () => {
    expect(isTerminal("completed")).toBe(true);
    expect(isTerminal("failed")).toBe(true);
    expect(isTerminal("cancelled")).toBe(true);
    expect(isTerminal("queued")).toBe(false);
    expect(isTerminal("dispatched")).toBe(false);
    expect(isTerminal("running")).toBe(false);
  });
});

describe("transitionTurn", () => {
  test("dispatches a queued turn and stamps the time", () => {
    const result = transitionTurn(turn(), session(), { status: "dispatched" }, 50);
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({ status: "dispatched", updatedAt: 50 }),
    });
  });

  test("runs a dispatched turn", () => {
    const result = transitionTurn(
      turn({ status: "dispatched" }),
      session(),
      { status: "running" },
      60,
    );
    expect(result.ok && result.next.status).toBe("running");
  });

  test("requeues a dispatched turn whose gateway lost its claim", () => {
    const result = transitionTurn(
      turn({ status: "dispatched" }),
      session(),
      { status: "queued" },
      61,
    );
    expect(result.ok && result.next.status).toBe("queued");
  });

  test("completes a running turn with its stop reason and clears the error", () => {
    const result = transitionTurn(
      turn({ status: "running", error: "old failure" }),
      session(),
      { status: "completed", stopReason: "end_turn" },
      70,
    );
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({
        status: "completed",
        stopReason: "end_turn",
        error: null,
        updatedAt: 70,
      }),
    });
  });

  test("fails a running turn with its error and clears the stop reason", () => {
    const result = transitionTurn(
      turn({ status: "running", stopReason: "stale" }),
      session(),
      { status: "failed", error: "gateway crashed" },
      71,
    );
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({
        status: "failed",
        error: "gateway crashed",
        stopReason: null,
      }),
    });
  });

  test("cancels a running turn and clears both fields", () => {
    const result = transitionTurn(
      turn({ status: "running", stopReason: "x", error: "y" }),
      session(),
      { status: "cancelled" },
      72,
    );
    expect(result).toEqual({
      ok: true,
      next: expect.objectContaining({ status: "cancelled", stopReason: null, error: null }),
    });
  });

  test("cancels a queued turn before it runs", () => {
    const result = transitionTurn(turn(), session(), { status: "cancelled" }, 73);
    expect(result.ok && result.next.status).toBe("cancelled");
  });

  test("refuses to advance a turn that already finished", () => {
    for (const status of TERMINAL_TURN_STATUSES) {
      expect(transitionTurn(turn({ status }), session(), { status: "running" })).toEqual({
        ok: false,
        reason: "already-terminal",
      });
    }
  });

  test("refuses every move in a closed session", () => {
    expect(transitionTurn(turn(), session({ status: "closed" }), { status: "dispatched" })).toEqual(
      {
        ok: false,
        reason: "session-closed",
      },
    );
  });

  test("reports a terminal check before a session check", () => {
    expect(
      transitionTurn(turn({ status: "completed" }), session({ status: "closed" }), {
        status: "running",
      }),
    ).toEqual({ ok: false, reason: "already-terminal" });
  });

  test("refuses a move the table does not list", () => {
    expect(transitionTurn(turn(), session(), { status: "running" })).toEqual({
      ok: false,
      reason: "illegal-transition",
    });
    expect(transitionTurn(turn({ status: "running" }), session(), { status: "queued" })).toEqual({
      ok: false,
      reason: "illegal-transition",
    });
    expect(
      transitionTurn(turn({ status: "running" }), session(), { status: "dispatched" }),
    ).toEqual({
      ok: false,
      reason: "illegal-transition",
    });
  });

  test("only listed transitions are legal, over every pair", () => {
    const statuses = Object.keys(TURN_TRANSITIONS) as TurnStatus[];
    for (const from of statuses) {
      for (const to of statuses) {
        const update = updateFor(to);
        const result = transitionTurn(turn({ status: from }), session(), update);
        const expected = TURN_TRANSITIONS[from].includes(to);
        expect(result.ok).toBe(expected);
      }
    }
  });

  test("the turn a transition returns is the one a caller stores", () => {
    fc.assert(
      fc.property(fc.constantFrom(...(Object.keys(TURN_TRANSITIONS) as TurnStatus[])), (from) => {
        const result = transitionTurn(turn({ status: from, prompt: "keep me" }), session(), {
          status: "cancelled",
        });
        if (!result.ok) return;
        expect(result.next.prompt).toBe("keep me");
        expect(result.next.id).toBe("t1");
        expect(result.next.idempotencyKey).toBe("evt-1");
        expect(result.next.status).toBe("cancelled");
      }),
    );
  });

  test("no sequence of updates ever drives a turn into an unknown status", () => {
    const updates: TurnUpdate[] = [
      { status: "dispatched" },
      { status: "running" },
      { status: "queued" },
      { status: "completed", stopReason: "end_turn" },
      { status: "failed", error: "e" },
      { status: "cancelled" },
    ];
    const statuses = new Set<string>(Object.keys(TURN_TRANSITIONS));
    fc.assert(
      fc.property(fc.array(fc.constantFrom(...updates), { maxLength: 12 }), (sequence) => {
        let current = turn();
        for (const update of sequence) {
          const result = transitionTurn(current, session(), update);
          if (!result.ok) continue;
          current = result.next;
          expect(statuses.has(current.status)).toBe(true);
        }
      }),
    );
  });
});

function updateFor(status: TurnStatus): TurnUpdate {
  switch (status) {
    case "completed":
      return { status: "completed", stopReason: "end_turn" };
    case "failed":
      return { status: "failed", error: "e" };
    case "dispatched":
    case "running":
    case "queued":
    case "cancelled":
      return { status };
  }
}

describe("nextQueued", () => {
  test("returns the oldest queued turn", () => {
    const turns = [
      turn({ id: "b", createdAt: 20 }),
      turn({ id: "a", createdAt: 10 }),
      turn({ id: "c", status: "running", createdAt: 5 }),
    ];
    expect(nextQueued(turns)?.id).toBe("a");
  });

  test("breaks a timestamp tie by id so the order is deterministic", () => {
    const turns = [turn({ id: "b", createdAt: 10 }), turn({ id: "a", createdAt: 10 })];
    expect(nextQueued(turns)?.id).toBe("a");
    expect(nextQueued([...turns].reverse())?.id).toBe("a");
  });

  test("returns undefined when nothing is queued", () => {
    expect(nextQueued([])).toBeUndefined();
    expect(nextQueued([turn({ status: "running" })])).toBeUndefined();
    expect(
      nextQueued([turn({ status: "completed" }), turn({ status: "dispatched" })]),
    ).toBeUndefined();
  });

  test("the answer is one of the queued turns, and its timestamp is the smallest", () => {
    const anyTurn = fc.record({
      id: fc.string({ minLength: 1 }),
      status: fc.constantFrom(...(Object.keys(TURN_TRANSITIONS) as TurnStatus[])),
      createdAt: fc.integer({ min: 0, max: 5 }),
    });
    fc.assert(
      fc.property(fc.array(anyTurn), (drafts) => {
        const turns = drafts.map((draft) => turn(draft));
        const picked = nextQueued(turns);
        const queued = turns.filter((candidate) => candidate.status === "queued");
        if (queued.length === 0) {
          expect(picked).toBeUndefined();
          return;
        }
        const oldest = Math.min(...queued.map((candidate) => candidate.createdAt));
        expect(picked?.status).toBe("queued");
        expect(picked?.createdAt).toBe(oldest);
        const tie = queued.filter((candidate) => candidate.createdAt === oldest);
        const pickedId = picked?.id ?? "";
        expect(tie.some((candidate) => candidate.id === pickedId)).toBe(true);
        expect(tie.filter((other) => other.id < pickedId)).toEqual([]);
      }),
    );
  });
});

describe("acceptsTurns", () => {
  test("an active session accepts turns and a closed one does not", () => {
    expect(acceptsTurns(session())).toBe(true);
    expect(acceptsTurns(session({ status: "closed" }))).toBe(false);
  });
});

describe("table contents", () => {
  test("every listed transition is a status name", () => {
    for (const [from, targets] of Object.entries(TURN_TRANSITIONS)) {
      expect(Object.keys(TURN_TRANSITIONS)).toContain(from);
      for (const target of targets) expect(Object.keys(TURN_TRANSITIONS)).toContain(target);
    }
  });

  test("a dispatched turn may be requeued, which is how a lost claim recovers", () => {
    expect(TURN_TRANSITIONS.dispatched).toContain("queued");
    const result = transitionTurn(turn({ status: "dispatched" }), session(), { status: "queued" });
    expect(result.ok && result.next.status).toBe("queued");
  });

  test("the tie-break picks the smaller id at equal timestamps, in either input order", () => {
    const a = turn({ id: "aaa", createdAt: 7 });
    const b = turn({ id: "bbb", createdAt: 7 });
    expect(nextQueued([b, a])?.id).toBe("aaa");
    expect(nextQueued([a, b])?.id).toBe("aaa");
    expect(nextQueued([a, b, turn({ id: "aab", createdAt: 7 })])?.id).toBe("aaa");
  });
});

describe("dispatch order", () => {
  test("the arrival time wins over the id", () => {
    const late = turn({ id: "aaa", createdAt: 9 });
    const early = turn({ id: "zzz", createdAt: 2 });
    expect(nextQueued([late, early])?.id).toBe("zzz");
  });

  test("the id breaks a tie in ascending code-unit order", () => {
    expect(nextQueued([turn({ id: "b", createdAt: 1 }), turn({ id: "a", createdAt: 1 })])?.id).toBe(
      "a",
    );
  });
});

describe("duplicate arrivals", () => {
  test("two turns with one timestamp and one id keep the first arrival", () => {
    const first = turn({ id: "dup", createdAt: 3 });
    const second = turn({ id: "dup", createdAt: 3, prompt: "second" });
    expect(nextQueued([first, second])).toBe(first);
    expect(nextQueued([second, first])).toBe(second);
  });

  test("a later arrival of the same timestamp but a smaller id wins", () => {
    const later = turn({ id: "a", createdAt: 3 });
    const earlier = turn({ id: "b", createdAt: 3 });
    expect(nextQueued([earlier, later])).toBe(later);
  });
});
