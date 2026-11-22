import { describe, expect, test } from "bun:test";
import { SessionService } from "./session-service.ts";
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
