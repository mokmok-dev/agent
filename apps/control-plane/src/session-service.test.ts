import { describe, expect, test } from "bun:test";
import { SessionService } from "./session-service.ts";
import { unavailableRunner } from "./turn-runner.ts";
import { indexKey } from "./thread-index.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { durableStorage, type StorageHost } from "./storage.ts";
import { MissingSession, threadKey, type ThreadRef } from "@agent/store";

class Host implements StorageHost {
  readonly values = new Map<string, string>();
  writes = 0;

  async get<T>(key: string): Promise<T | undefined> {
    const raw = this.values.get(key);
    return raw === undefined ? undefined : (JSON.parse(raw) as T);
  }

  async put<T>(key: string, value: T): Promise<void> {
    this.writes++;
    this.values.set(key, JSON.stringify(value));
  }

  async delete(key: string): Promise<void> {
    this.values.delete(key);
  }

  async list<T>(options: { prefix: string }): Promise<Map<string, T>> {
    return new Map(
      [...this.values.entries()]
        .filter(([key]) => key.startsWith(options.prefix))
        .map(([key, raw]) => [key, JSON.parse(raw) as T]),
    );
  }
}

const thread: ThreadRef = {
  provider: "slack",
  workspaceId: "T1",
  channelId: "C1",
  threadId: "1.5",
};

const model = { provider: "cloudflare-workers-ai", modelId: "m" };

const service = () => {
  const host = new Host();
  return { host, service: new SessionService(durableStorage(host)) };
};

describe("storage adapter", () => {
  test("maps every storage operation", async () => {
    const host = new Host();
    const storage = durableStorage(host);
    await storage.put("a", { n: 1 });
    expect(await storage.get("a")).toEqual({ n: 1 });
    expect(await storage.list("a")).toEqual([{ key: "a", value: { n: 1 } }]);
    await storage.delete("a");
    expect(await storage.get("a")).toBeUndefined();
  });
});

describe("thread index", () => {
  test("the index key names the thread", () => {
    expect(indexKey(thread)).toBe("thread:slack:T1:C1:1.5");
  });

  test("the session id is the thread key, so it is stable across restarts", () => {
    expect(threadKey(thread)).toBe("slack:T1:C1:1.5");
    expect(threadKey({ ...thread, threadId: "other" })).not.toBe(threadKey(thread));
  });

  test("lookup assigns once and returns the same id after that", async () => {
    const host = new Host();
    const index = new ThreadIndexObject(host);
    const first = await index.lookup(thread);
    const second = await index.lookup(thread);
    expect(first).toBe("slack:T1:C1:1.5");
    expect(second).toBe(first);
  });

  test("forget makes the next lookup assign again", async () => {
    const host = new Host();
    const index = new ThreadIndexObject(host);
    await index.lookup(thread);
    await index.forget(thread);
    expect(host.values.has(indexKey(thread))).toBe(false);
    expect(await index.lookup(thread)).toBe("slack:T1:C1:1.5");
  });
});

describe("running a turn", () => {
  const queued = async (sessions: ReturnType<typeof service>["service"]) => {
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    if (outcome.kind === "refused") throw new Error(outcome.refusal);
    return outcome.turn;
  };

  const runner = (
    result: { kind: "answered"; text: string } | { kind: "unanswered"; reason: string },
  ) => {
    const inputs: unknown[] = [];
    return {
      inputs,
      executions: () => inputs.length,
      run: async (input: unknown) => {
        inputs.push(input);
        return result;
      },
    };
  };

  test("an answered turn moves to completed and returns the text", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    const answer = runner({ kind: "answered", text: "the answer" });
    const outcome = await sessions.run("s1", turn.id, "conv-1", model, answer, 20);
    expect(outcome).toEqual({ kind: "answered", text: "the answer" });
    expect((await sessions.turns("s1"))[0]?.status).toBe("completed");
  });

  test("the runner sees the prompt, the operation id, and the chosen model", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    const answer = runner({ kind: "answered", text: "x" });
    await sessions.run("s1", turn.id, "conv-9", model, answer, 20);
    expect(answer.inputs[0]).toEqual({
      sessionId: "s1",
      conversationId: "conv-9",
      prompt: "hello",
      operationId: "evt-1",
      model,
      now: 20,
    });
  });

  test("the runner runs after the turn is dispatched, so a crash leaves evidence", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    let statusWhenRun: string | undefined;
    const observed = {
      run: async () => {
        statusWhenRun = (await sessions.turns("s1"))[0]?.status;
        return { kind: "answered" as const, text: "x" };
      },
    };
    await sessions.run("s1", turn.id, "c", model, observed, 20);
    expect(statusWhenRun).toBe("dispatched");
  });

  test("an unanswered run fails the turn with the runner's reason", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    const outcome = await sessions.run(
      "s1",
      turn.id,
      "c",
      model,
      runner({ kind: "unanswered", reason: "no key" }),
      20,
    );
    expect(outcome).toEqual({ kind: "failed", error: "no key" });
    const [run] = await sessions.turns("s1");
    expect(run?.status).toBe("failed");
    expect(run?.error).toBe("no key");
  });

  test("a turn that already ran is not run again", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    const answer = runner({ kind: "answered", text: "x" });
    await sessions.run("s1", turn.id, "c", model, answer, 20);
    const again = await sessions.run("s1", turn.id, "c", model, answer, 21);
    expect(again).toEqual({ kind: "not-queued", status: "completed" });
    expect(answer.inputs).toHaveLength(1);
  });

  test("a completed turn records the reason the model stopped", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    await sessions.run("s1", turn.id, "c", model, runner({ kind: "answered", text: "x" }), 20);
    expect((await sessions.turns("s1"))[0]?.stopReason).toBe("stop");
  });

  test("a turn that is not queued is refused without running", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    await sessions.transition("s1", turn.id, { status: "cancelled" }, 11);
    const answer = runner({ kind: "answered", text: "x" });
    const outcome = await sessions.run("s1", turn.id, "c", model, answer, 20);
    expect(outcome).toEqual({ kind: "not-queued", status: "cancelled" });
    expect(answer.inputs).toEqual([]);
  });

  test("a turn id the session does not have is missing", async () => {
    const { service: sessions } = service();
    await queued(sessions);
    const outcome = await sessions.run(
      "s1",
      "nope",
      "c",
      model,
      runner({ kind: "answered", text: "x" }),
      20,
    );
    expect(outcome).toEqual({ kind: "missing" });
  });

  test("a cancelled turn is refused before the runner is asked", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    await sessions.transition("s1", turn.id, { status: "cancelled" }, 11);
    const answer = runner({ kind: "answered", text: "x" });
    expect(await sessions.run("s1", turn.id, "c", model, answer, 20)).toEqual({
      kind: "not-queued",
      status: "cancelled",
    });
    expect(answer.executions()).toBe(0);
  });

  test("a turn a race already claimed is not run twice", async () => {
    const { service: sessions } = service();
    const turn = await queued(sessions);
    const answer = runner({ kind: "answered", text: "x" });
    const [first, second] = await Promise.all([
      sessions.run("s1", turn.id, "c", model, answer, 20),
      sessions.run("s1", turn.id, "c", model, answer, 20),
    ]);
    expect([first.kind, second.kind].sort()).toEqual(["answered", "not-queued"]);
    expect(answer.executions()).toBe(1);
  });
});

describe("the runner a deployment without an agent gets", () => {
  test("refuses every turn and names the reason, so the turn fails rather than hangs", async () => {
    const outcome = await unavailableRunner("no agent is configured").run({
      sessionId: "s",
      conversationId: "c",
      prompt: "hi",
      operationId: "o",
      model,
      now: 0,
    });
    expect(outcome).toEqual({ kind: "unanswered", reason: "no agent is configured" });
  });
});

describe("delivery", () => {
  test("the first delivery creates the session and queues its turn", async () => {
    const { service: sessions } = service();
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    expect(outcome.kind).toBe("accepted");
    expect(outcome.kind === "accepted" && outcome.session.id).toBe("s1");
    expect(outcome.kind === "accepted" && outcome.turn.status).toBe("queued");
    expect(outcome.kind === "accepted" && outcome.turn.prompt).toBe("hello");
  });

  test("a delivery records the route the session will run on", async () => {
    const { service: sessions } = service();
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    expect(outcome.kind).toBe("accepted");
    if (outcome.kind === "refused") return;
    expect(outcome.selection).toMatchObject({
      agent: "pi",
      runtime: "cloudflare-sandbox",
      protocol: "durable",
    });
    const session = await sessions.session("s1");
    expect(session?.selection).toEqual(outcome.selection);
    expect(session?.conversationId).toBe("s1");
  });

  test("an explicit agent and runtime are honored and kept on the session", async () => {
    const { service: sessions } = service();
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 10, {
      agent: "claude-code",
      runtime: "local-vm",
    });
    if (outcome.kind === "refused") throw new Error(outcome.refusal);
    expect(outcome.selection).toMatchObject({
      agent: "claude-code",
      runtime: "local-vm",
      protocol: "stdio",
    });
  });

  test("an unroutable request is refused and leaves no turn behind", async () => {
    const { service: sessions } = service();
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 10, {
      agent: "codex",
      runtime: "local-vm",
    });
    expect(outcome).toEqual({
      kind: "refused",
      reason: "unroutable",
      refusal: "unsupported-combination",
    });
    expect(await sessions.turns("s1")).toEqual([]);
  });

  test("a refused request can be retried with a runtime that supports the agent", async () => {
    const { service: sessions } = service();
    await sessions.deliver("s1", thread, "hello", "evt-1", 10, {
      agent: "codex",
      runtime: "local-vm",
    });
    const outcome = await sessions.deliver("s1", thread, "hello", "evt-1", 11, {
      agent: "codex",
      runtime: "cloudflare-sandbox",
    });
    expect(outcome.kind).toBe("accepted");
  });

  test("a session keeps the route it first chose, even when a replay asks for another", async () => {
    const { service: sessions } = service();
    const first = await sessions.deliver("s1", thread, "hello", "evt-1", 10, { agent: "pi" });
    const second = await sessions.deliver("s1", thread, "hello", "evt-2", 11, {
      agent: "grok-build",
      runtime: "local-vm",
    });
    if (first.kind === "refused" || second.kind === "refused")
      throw new Error("unexpected refusal");
    expect(second.selection).toEqual(first.selection);
    expect(second.selection.agent).toBe("pi");
  });

  test("a replayed delivery returns the turn it already created", async () => {
    const { service: sessions } = service();
    const first = await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    const second = await sessions.deliver("s1", thread, "changed", "evt-1", 11);
    expect(first.kind).toBe("accepted");
    expect(second.kind).toBe("replayed");
    if (first.kind === "refused" || second.kind === "refused") return;
    expect(second.turn.id).toBe(first.turn.id);
    expect(second.turn.prompt).toBe("hello");
    expect(second.selection).toEqual(first.selection);
  });

  test("two concurrent deliveries for one thread converge on one turn", async () => {
    const { service: sessions } = service();
    const [left, right] = await Promise.all([
      sessions.deliver("s1", thread, "hi", "same-event", 10),
      sessions.deliver("s1", thread, "hi", "same-event", 11),
    ]);
    if (left.kind === "refused" || right.kind === "refused") return;
    expect(left.turn.id).toBe(right.turn.id);
    expect((await sessions.turns("s1")).length).toBe(1);
  });

  test("a closed session refuses a delivery", async () => {
    const { service: sessions } = service();
    await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    await sessions.close("s1", 11);
    const outcome = await sessions.deliver("s1", thread, "again", "evt-2", 12);
    expect(outcome).toEqual({ kind: "refused", reason: "closed" });
  });

  test("closing a missing session throws MissingSession", async () => {
    const { service: sessions } = service();
    expect(await sessions.close("nope", 1).catch((error: unknown) => error)).toBeInstanceOf(
      MissingSession,
    );
  });

  test("binding a missing session throws MissingSession", async () => {
    const { service: sessions } = service();
    expect(
      await sessions.bind("nope", "a", "g", 1).catch((error: unknown) => error),
    ).toBeInstanceOf(MissingSession);
  });

  test("binding records the agent session and the gateway", async () => {
    const { service: sessions } = service();
    await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    const bound = await sessions.bind("s1", "acp-9", "gateway-2", 11);
    expect(bound).toEqual(
      expect.objectContaining({ acpSessionId: "acp-9", gatewayId: "gateway-2", updatedAt: 11 }),
    );
  });

  test("a delivery for an existing session reuses its record", async () => {
    const { service: sessions } = service();
    await sessions.deliver("s1", thread, "one", "evt-1", 10);
    const second = await sessions.deliver("s1", thread, "two", "evt-2", 20);
    expect(second.kind === "refused" ? undefined : second.session.createdAt).toBe(10);
    expect((await sessions.turns("s1")).length).toBe(2);
  });
});

describe("turns", () => {
  const withTurn = async () => {
    const sessions = service();
    await sessions.service.deliver("s1", thread, "hello", "evt-1", 10);
    return sessions;
  };

  test("next returns the queued turn", async () => {
    const { service: sessions } = await withTurn();
    expect((await sessions.next("s1"))?.id).toBe("t1");
  });

  test("active prefers a live turn over a queued one", async () => {
    const { service: sessions } = await withTurn();
    await sessions.transition("s1", "t1", { status: "dispatched" }, 11);
    expect((await sessions.active("s1"))?.status).toBe("dispatched");
  });

  test("active falls back to the oldest queued turn when nothing is live", async () => {
    const { service: sessions } = await withTurn();
    expect((await sessions.active("s1"))?.id).toBe("t1");
  });

  test("active is undefined when nothing is pending", async () => {
    const { service: sessions } = await withTurn();
    await sessions.transition("s1", "t1", { status: "cancelled" }, 11);
    expect(await sessions.active("s1")).toBeUndefined();
  });

  test("a transition on a missing session throws MissingSession", async () => {
    const { service: sessions } = service();
    expect(
      await sessions
        .transition("nope", "t1", { status: "dispatched" }, 1)
        .catch((error: unknown) => error),
    ).toBeInstanceOf(MissingSession);
  });

  test("a transition on a missing turn reports not-found", async () => {
    const { service: sessions } = await withTurn();
    expect(await sessions.transition("s1", "nope", { status: "dispatched" }, 1)).toEqual({
      ok: false,
      reason: "not-found",
    });
  });

  test("a legal transition moves the stored turn", async () => {
    const { service: sessions } = await withTurn();
    await sessions.transition("s1", "t1", { status: "dispatched" }, 11);
    expect((await sessions.turns("s1"))[0]?.status).toBe("dispatched");
  });

  test("a session read returns the stored record", async () => {
    const { service: sessions } = await withTurn();
    expect((await sessions.session("s1"))?.thread).toEqual(thread);
  });
});

describe("durable object wrappers", () => {
  test("the session object delegates every method", async () => {
    const host = new Host();
    const object = new SessionObject(host);
    const outcome = await object.deliver("s1", thread, "hello", "evt-1", 10);
    expect(outcome.kind).toBe("accepted");
    expect((await object.session("s1"))?.id).toBe("s1");
    expect((await object.turns("s1")).length).toBe(1);
    expect((await object.next("s1"))?.id).toBe("t1");
    expect((await object.active("s1"))?.id).toBe("t1");
    expect(await object.transition("s1", "t1", { status: "dispatched" }, 11)).toMatchObject({
      ok: true,
    });
    expect(await object.bind("s1", "acp-1", "g", 12)).toMatchObject({ acpSessionId: "acp-1" });
    expect(await object.close("s1", 13)).toMatchObject({ status: "closed" });
  });
});

describe("index writes", () => {
  test("a lookup stores the id it returns", async () => {
    const host = new Host();
    const index = new ThreadIndexObject(host);
    const id = await index.lookup(thread);
    expect(await host.get(indexKey(thread))).toBe(id);
  });

  test("a second lookup leaves the stored id untouched", async () => {
    const host = new Host();
    const index = new ThreadIndexObject(host);
    await index.lookup(thread);
    const writes = host.writes;
    await index.lookup(thread);
    expect(host.writes).toBe(writes + 1);
  });
});

describe("the work a runner is handed", () => {
  test("runNext takes the oldest queued turn and the route its session chose", async () => {
    const { service: sessions } = service();
    const delivered = await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    if (delivered.kind === "refused") throw new Error(delivered.refusal);
    const turn = delivered.turn;
    const seen: unknown[] = [];
    const outcome = await sessions.runNext(
      "s1",
      {
        run: async (input) => {
          seen.push(input);
          return { kind: "answered", text: "ok" };
        },
      },
      20,
    );
    expect(outcome).toEqual({ kind: "answered", text: "ok" });
    expect(seen[0]).toMatchObject({ sessionId: "s1", prompt: turn.prompt });
  });

  test("runNext does nothing when no turn is queued", async () => {
    const { service: sessions } = service();
    expect(
      await sessions.runNext("s1", { run: async () => ({ kind: "answered", text: "" }) }, 1),
    ).toBeUndefined();
  });

  test("runNext refuses to guess a model when the session was never routed", async () => {
    const host = new Host();
    const sessions = new SessionService(durableStorage(host));
    // A turn written without a route is a state the router cannot produce; the
    // runner must not invent a model for it.
    await host.put("session:s1", {
      id: "s1",
      thread: { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
      status: "active",
      acpSessionId: null,
      gatewayId: null,
      selection: null,
      conversationId: null,
      turnSeq: 1,
      createdAt: 0,
      updatedAt: 0,
    });
    await host.put("turn:s1:t1", {
      id: "t1",
      sessionId: "s1",
      seq: 1,
      prompt: "hello",
      status: "queued",
      idempotencyKey: "evt-1",
      stopReason: null,
      error: null,
      createdAt: 0,
      updatedAt: 0,
    });
    expect(
      sessions.runNext("s1", { run: async () => ({ kind: "answered", text: "" }) }, 1),
    ).rejects.toThrow("has a turn but no route");
  });
});

describe("the conversation a run uses", () => {
  test("a routed session runs in the conversation the router stored", async () => {
    const host = new Host();
    const sessions = new SessionService(durableStorage(host), {
      conversationOf: () => "conv-9",
    });
    await sessions.deliver("s1", thread, "hello", "evt-1", 10);
    const seen: { conversationId?: string }[] = [];
    await sessions.runNext(
      "s1",
      {
        run: async (input) => {
          seen.push(input);
          return { kind: "answered", text: "ok" };
        },
      },
      20,
    );
    expect(seen[0]?.conversationId).toBe("conv-9");
  });
});
