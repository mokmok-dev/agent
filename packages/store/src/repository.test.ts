import { describe, expect, test } from "bun:test";
import fc from "fast-check";
import {
  idemKey,
  MissingSession,
  SessionRepository,
  sessionKey,
  turnKey,
  turnPrefix,
  type Storage,
} from "./repository.ts";
import type { ThreadRef, TurnStatus, TurnUpdate } from "./session.ts";

class MemoryStorage implements Storage {
  readonly values = new Map<string, string>();

  async get<T>(key: string): Promise<T | undefined> {
    const raw = this.values.get(key);
    return raw === undefined ? undefined : (JSON.parse(raw) as T);
  }

  async put<T>(key: string, value: T): Promise<void> {
    this.values.set(key, JSON.stringify(value));
  }

  async delete(key: string): Promise<void> {
    this.values.delete(key);
  }

  async list<T>(prefix: string): Promise<Array<{ key: string; value: T }>> {
    return [...this.values.entries()]
      .filter(([key]) => key.startsWith(prefix))
      .map(([key, raw]) => ({ key, value: JSON.parse(raw) as T }));
  }
}

const thread: ThreadRef = {
  provider: "slack",
  workspaceId: "T1",
  channelId: "C1",
  threadId: "1.5",
};

const repository = () => {
  const storage = new MemoryStorage();
  return { storage, repo: new SessionRepository(storage) };
};

describe("keys", () => {
  test("are namespaced by session and purpose", () => {
    expect(sessionKey("s1")).toBe("session:s1");
    expect(turnPrefix("s1")).toBe("turn:s1:");
    expect(turnKey("s1", "t1")).toBe("turn:s1:t1");
    expect(idemKey("s1", "evt")).toBe("idem:s1:evt");
  });

  test("a turn prefix only matches that session's turns", async () => {
    const { storage, repo } = repository();
    await repo.ensureSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "e1", 1);
    await repo.ensureSession("s2", thread, 0);
    await repo.appendTurn("s2", "b", "e2", 1);
    expect((await storage.list("turn:s1:")).length).toBe(1);
  });
});

describe("sessions", () => {
  test("a new session is active with no agent binding", async () => {
    const { repo } = repository();
    const session = await repo.createSession("s1", thread, 5);
    expect(session).toEqual({
      id: "s1",
      thread,
      status: "active",
      acpSessionId: null,
      gatewayId: null,
      selection: null,
      conversationId: null,
      turnSeq: 0,
      createdAt: 5,
      updatedAt: 5,
    });
  });

  test("ensureSession creates once and returns the stored session after that", async () => {
    const { repo } = repository();
    const first = await repo.ensureSession("s1", thread, 5);
    const second = await repo.ensureSession("s1", { ...thread, threadId: "other" }, 99);
    expect(second).toEqual(first);
    expect(second.thread.threadId).toBe("1.5");
  });

  test("a missing session reads as undefined", async () => {
    const { repo } = repository();
    expect(await repo.session("nope")).toBeUndefined();
  });

  test("closing a session sets it closed and stamps the time", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const result = await repo.close("s1", 8);
    expect(result).toEqual(expect.objectContaining({ status: "closed", updatedAt: 8 }));
    expect((await repo.session("s1"))?.status).toBe("closed");
  });

  test("closing a missing session throws MissingSession", async () => {
    const { repo } = repository();
    expect(await repo.close("nope", 1).catch((error: unknown) => error)).toBeInstanceOf(
      MissingSession,
    );
  });

  test("binding an agent session records the id and the gateway", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const result = await repo.bindAgentSession("s1", "acp-1", "gateway-1", 9);
    expect(result).toEqual(
      expect.objectContaining({ acpSessionId: "acp-1", gatewayId: "gateway-1", updatedAt: 9 }),
    );
    expect((await repo.session("s1"))?.acpSessionId).toBe("acp-1");
  });

  test("routing records the choice and the conversation", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const selection = {
      agent: "pi",
      runtime: "cloudflare-sandbox",
      model: { provider: "cloudflare-workers-ai", modelId: "m" },
      protocol: "durable",
    };
    const routed = await repo.route("s1", selection, "conv-1", 6);
    expect(routed).toEqual(
      expect.objectContaining({ selection, conversationId: "conv-1", updatedAt: 6 }),
    );
    expect((await repo.session("s1"))?.conversationId).toBe("conv-1");
  });

  test("a routed session keeps its first choice", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const first = {
      agent: "pi",
      runtime: "cloudflare-sandbox",
      model: { provider: "p", modelId: "m" },
      protocol: "durable",
    };
    const other = { ...first, agent: "codex" };
    await repo.route("s1", first, "conv-1", 6);
    const again = await repo.route("s1", other, "conv-2", 7);
    expect(again.selection).toEqual(first);
    expect(again.conversationId).toBe("conv-1");
  });

  test("routing a missing session throws MissingSession", async () => {
    const { repo } = repository();
    const refusal = await repo
      .route(
        "nope",
        {
          agent: "pi",
          runtime: "local-vm",
          model: { provider: "p", modelId: "m" },
          protocol: "stdio",
        },
        "c",
        1,
      )
      .catch((error: unknown) => error);
    expect(refusal).toBeInstanceOf(MissingSession);
  });

  test("binding a missing session throws MissingSession", async () => {
    const { repo } = repository();
    expect(
      await repo.bindAgentSession("nope", "a", "g", 1).catch((error: unknown) => error),
    ).toBeInstanceOf(MissingSession);
  });
});

describe("turns", () => {
  test("appendTurn stores a queued turn and advances the sequence", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    const result = await repo.appendTurn("s1", "do it", "evt-1", 10);
    expect(result).toEqual({
      kind: "appended",
      turn: {
        id: "t1",
        sessionId: "s1",
        prompt: "do it",
        status: "queued",
        idempotencyKey: "evt-1",
        stopReason: null,
        error: null,
        createdAt: 10,
        updatedAt: 10,
      },
    });
    expect((await repo.session("s1"))?.turnSeq).toBe(1);
  });

  test("turn ids continue across appends", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "e1", 1);
    await repo.appendTurn("s1", "b", "e2", 2);
    expect((await repo.turns("s1")).map((turn) => turn.id)).toEqual(["t1", "t2"]);
  });

  test("a replayed delivery returns the turn it already created", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    const first = await repo.appendTurn("s1", "a", "same", 1);
    const second = await repo.appendTurn("s1", "a changed", "same", 2);
    expect(first.kind).toBe("appended");
    expect(second.kind).toBe("replayed");
    expect(second.kind === "replayed" && second.turn.prompt).toBe("a");
    expect((await repo.turns("s1")).length).toBe(1);
  });

  test("a replayed delivery never advances the sequence", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "same", 1);
    await repo.appendTurn("s1", "a", "same", 2);
    expect((await repo.session("s1"))?.turnSeq).toBe(1);
  });

  test("different deliveries create different turns", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "one", 1);
    await repo.appendTurn("s1", "b", "two", 2);
    expect((await repo.turns("s1")).map((turn) => turn.prompt)).toEqual(["a", "b"]);
  });

  test("nextTurn returns the oldest queued turn", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "first", "e1", 10);
    await repo.appendTurn("s1", "second", "e2", 20);
    expect((await repo.nextTurn("s1"))?.prompt).toBe("first");
  });

  test("nextTurn is undefined for a session with no queued turn", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    expect(await repo.nextTurn("s1")).toBeUndefined();
  });
});

describe("transitions", () => {
  const withTurn = async () => {
    const { storage, repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "e1", 1);
    return { storage, repo };
  };

  test("the lifecycle advances through the table", async () => {
    const { repo } = await withTurn();
    expect(await repo.transition("s1", "t1", { status: "dispatched" }, 2)).toMatchObject({
      ok: true,
      next: { status: "dispatched" },
    });
    expect(await repo.transition("s1", "t1", { status: "running" }, 3)).toMatchObject({
      ok: true,
      next: { status: "running" },
    });
    expect(
      await repo.transition("s1", "t1", { status: "completed", stopReason: "end_turn" }, 4),
    ).toMatchObject({ ok: true, next: { status: "completed", stopReason: "end_turn" } });
  });

  test("an illegal move leaves storage untouched", async () => {
    const { storage, repo } = await withTurn();
    const before = storage.values.get(turnKey("s1", "t1"));
    expect(await repo.transition("s1", "t1", { status: "running" }, 2)).toEqual({
      ok: false,
      reason: "illegal-transition",
    });
    expect(storage.values.get(turnKey("s1", "t1"))).toBe(before);
  });

  test("a closed session refuses every move", async () => {
    const { repo } = await withTurn();
    await repo.close("s1", 5);
    expect(await repo.transition("s1", "t1", { status: "dispatched" }, 6)).toEqual({
      ok: false,
      reason: "session-closed",
    });
  });

  test("a finished turn refuses every move", async () => {
    const { repo } = await withTurn();
    await repo.transition("s1", "t1", { status: "cancelled" }, 2);
    expect(await repo.transition("s1", "t1", { status: "dispatched" }, 3)).toEqual({
      ok: false,
      reason: "already-terminal",
    });
  });

  test("a missing session throws and a missing turn reports not-found", async () => {
    const { repo } = await withTurn();
    expect(
      await repo
        .transition("nope", "t1", { status: "dispatched" }, 1)
        .catch((error: unknown) => error),
    ).toBeInstanceOf(MissingSession);
    expect(await repo.transition("s1", "nope", { status: "dispatched" }, 1)).toEqual({
      ok: false,
      reason: "not-found",
    });
  });

  test("replaying a delivery never creates a second turn, over any sequence", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(fc.string({ minLength: 1 }), { maxLength: 8 }), async (keys) => {
        const { repo } = repository();
        await repo.createSession("s1", thread, 0);
        const firstKeySeen = new Set<string>();
        for (const key of keys) {
          const result = await repo.appendTurn("s1", `p-`, key, 1);
          expect(result.kind).toBe(firstKeySeen.has(key) ? "replayed" : "appended");
          firstKeySeen.add(key);
        }
        const distinct = firstKeySeen.size;
        expect((await repo.turns("s1")).length).toBe(distinct);
        expect((await repo.session("s1"))?.turnSeq).toBe(distinct);
      }),
    );
  });

  test("the stored turn always matches the last accepted transition", async () => {
    const updates: TurnUpdate[] = [
      { status: "dispatched" },
      { status: "running" },
      { status: "completed", stopReason: "end_turn" },
      { status: "failed", error: "e" },
      { status: "cancelled" },
      { status: "queued" },
    ];
    await fc.assert(
      fc.asyncProperty(
        fc.array(fc.constantFrom(...updates), { maxLength: 10 }),
        async (sequence: TurnUpdate[]) => {
          const { repo } = await withTurn();
          let expected: TurnStatus = "queued";
          for (const update of sequence) {
            const result = await repo.transition("s1", "t1", update, 1);
            if (result.ok) expected = result.next.status;
          }
          const stored = (await repo.turns("s1"))[0];
          expect(stored?.status).toBe(expected);
        },
      ),
    );
  });
});

describe("storage details", () => {
  test("a missing session throws MissingSession from a writer that needs it", async () => {
    const { repo } = repository();
    const error = await repo.appendTurn("nope", "p", "k", 1).catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(MissingSession);
    expect((error as MissingSession).message).toBe("no session nope");
    expect((error as MissingSession).name).toBe("MissingSession");
  });

  test("the idempotency marker holds the turn it created", async () => {
    const storage = new MemoryStorage();
    const repo = new SessionRepository(storage);
    await repo.createSession("s1", thread, 0);
    const result = await repo.appendTurn("s1", "p", "once", 1);
    expect(await storage.get(idemKey("s1", "once"))).toEqual(result.turn);
  });

  test("a claim takes a queued turn exactly once, even when two callers race", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const { turn } = await repo.appendTurn("s1", "hello", "evt-1", 6);
    const [first, second] = await Promise.all([
      repo.claimTurn("s1", turn.id, 7),
      repo.claimTurn("s1", turn.id, 7),
    ]);
    expect([first.ok, second.ok].sort()).toEqual([false, true]);
    expect((await repo.turns("s1"))[0]?.status).toBe("dispatched");
  });

  test("a claim on a turn that is not queued loses", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 5);
    const { turn } = await repo.appendTurn("s1", "hello", "evt-1", 6);
    await repo.transition("s1", turn.id, { status: "cancelled" }, 7);
    expect(await repo.claimTurn("s1", turn.id, 8)).toEqual({
      ok: false,
      reason: "already-terminal",
    });
  });

  test("a transition with no turn but a session reports not-found", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    expect(await repo.transition("s1", "t1", { status: "dispatched" }, 1)).toEqual({
      ok: false,
      reason: "not-found",
    });
  });

  test("turns maps the stored entries to their values", async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "a", "e1", 1);
    await repo.appendTurn("s1", "b", "e2", 2);
    expect((await repo.turns("s1")).map((turn) => turn.id)).toEqual(["t1", "t2"]);
  });

  test("the storage contract only needs get, put, delete, and list", async () => {
    const storage = new MemoryStorage();
    await storage.put("k", { a: 1 });
    expect(await storage.get<{ a: number }>("k")).toEqual({ a: 1 });
    expect(await storage.list<{ a: number }>("k")).toEqual([{ key: "k", value: { a: 1 } }]);
    await storage.delete("k");
    expect(await storage.get("k")).toBeUndefined();
    expect(await new SessionRepository(storage).session("k")).toBeUndefined();
  });
});

describe("activeTurn", () => {
  const withTurns = async () => {
    const { repo } = repository();
    await repo.createSession("s1", thread, 0);
    await repo.appendTurn("s1", "first", "e1", 10);
    await repo.appendTurn("s1", "second", "e2", 20);
    return repo;
  };

  test("prefers a running turn over a queued one", async () => {
    const repo = await withTurns();
    await repo.transition("s1", "t2", { status: "dispatched" }, 21);
    await repo.transition("s1", "t2", { status: "running" }, 22);
    expect((await repo.activeTurn("s1"))?.id).toBe("t2");
  });

  test("prefers a dispatched turn over a queued one", async () => {
    const repo = await withTurns();
    await repo.transition("s1", "t2", { status: "dispatched" }, 21);
    expect((await repo.activeTurn("s1"))?.id).toBe("t2");
  });

  test("falls back to the oldest queued turn", async () => {
    const repo = await withTurns();
    expect((await repo.activeTurn("s1"))?.id).toBe("t1");
  });

  test("is undefined when every turn is terminal", async () => {
    const repo = await withTurns();
    await repo.transition("s1", "t1", { status: "cancelled" }, 11);
    await repo.transition("s1", "t2", { status: "cancelled" }, 21);
    expect(await repo.activeTurn("s1")).toBeUndefined();
  });

  test("does not pick a completed turn", async () => {
    const repo = await withTurns();
    await repo.transition("s1", "t1", { status: "dispatched" }, 11);
    await repo.transition("s1", "t1", { status: "running" }, 12);
    await repo.transition("s1", "t1", { status: "completed", stopReason: "end_turn" }, 13);
    expect((await repo.activeTurn("s1"))?.id).toBe("t2");
  });
});
