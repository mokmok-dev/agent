import { describe, expect, test } from "bun:test";
import { accept } from "./worker.ts";
import type { Env } from "./worker.ts";
import type { ThreadRef, Turn } from "@agent/store";
import { OAUTH } from "./test-env.ts";
import { SessionObject } from "./session-do.ts";
import type { StorageHost } from "./storage.ts";
import { GATEWAY_CLAIM_DEADLINE_MS } from "@agent/store";

class FakeSessions {
  readonly calls: Array<{ method: string; args: unknown[] }> = [];
  active: Turn | undefined;
  /** What a run produced, or `undefined` for a deployment with no runner. */
  answerWith: { kind: "answered"; text: string } | { kind: "failed"; error: string } | undefined;
  /** Whether the delivery is refused before a run could happen. */
  refuse = false;

  active_(sessionId: string): Promise<Turn | undefined> {
    this.calls.push({ method: "active", args: [sessionId] });
    return Promise.resolve(this.active);
  }

  /** The session object's `accept`, which records the turn and then runs it. */
  adopt(...args: unknown[]): Promise<unknown> {
    this.calls.push({ method: "accept", args });
    if (this.refuse) return Promise.resolve({ kind: "refused", reason: "closed" });
    return Promise.resolve(
      this.answerWith === undefined
        ? { kind: "accepted" }
        : { kind: "accepted", run: this.answerWith },
    );
  }

  deliver(...args: unknown[]): Promise<unknown> {
    this.calls.push({ method: "deliver", args });
    return Promise.resolve({ kind: "accepted" });
  }

  transition(...args: unknown[]): Promise<unknown> {
    this.calls.push({ method: "transition", args });
    return Promise.resolve({ ok: true });
  }
}

const envWith = (
  sessions: FakeSessions,
  sessionIds: string[] = ["session-1"],
  overrides: Partial<Env> = {},
): Env => {
  const lookups: ThreadRef[] = [];
  const env: Env = {
    SLACK_SIGNING_SECRET: "s",
    SLACK_BOT_USER_ID: "B",
    DISCORD_PUBLIC_KEY: "p",
    LINEAR_WEBHOOK_SECRET: "l",
    ...OAUTH,
    LINEAR_OAUTH: { getByName: () => ({}) },
    THREAD_INDEX: {
      getByName: () => ({
        lookup: (thread: ThreadRef) => {
          lookups.push(thread);
          return Promise.resolve(sessionIds[lookups.length - 1] ?? sessionIds[0]!);
        },
      }),
    },
    SESSIONS: {
      getByName: () => ({
        active: (id: string) => sessions.active_(id),
        accept: (...args: unknown[]) => sessions.adopt(...args),
        transition: (...args: unknown[]) => sessions.transition(...args),
      }),
    },
    ...overrides,
  };
  return env;
};

const message = {
  kind: "message" as const,
  provider: "slack" as const,
  eventId: "Ev1",
  thread: { workspaceId: "T", channelId: "C", threadId: "1" },
  authorId: "U",
  text: "hello",
  replyTo: { channelId: "C", threadId: "1" },
  stage: "message" as const,
};

describe("the control plane to session relay", () => {
  test("a Linear start acknowledges before the turn is recorded", async () => {
    const sessions = new FakeSessions();
    const acks: string[] = [];
    const names: string[] = [];
    const replies = {
      acknowledge: async (sessionId: string) => {
        acks.push(sessionId);
        return { kind: "sent" as const, activityId: null };
      },
      answer: async () => ({ kind: "sent" as const, activityId: null }),
    };
    const env = envWith(sessions, ["session-1"], {
      LINEAR_OAUTH: {
        getByName: (name: string) => {
          names.push(name);
          return {
            status: async () => undefined,
            accessToken: async () => undefined,
          };
        },
      },
    });
    await accept(env, { ...message, provider: "linear", stage: "start" }, 1, replies);
    expect(acks).toEqual([message.thread.threadId]);
    expect(names).toEqual([]);
    expect(sessions.calls.map((call) => call.method)).toEqual(["accept"]);
  });

  test("without a reply port, a Linear start addresses the object named linear", async () => {
    const sessions = new FakeSessions();
    const names: string[] = [];
    const env = envWith(sessions, ["session-1"], {
      LINEAR_OAUTH: {
        getByName: (name: string) => {
          names.push(name);
          return {
            status: async () => undefined,
            accessToken: async () => undefined,
          };
        },
      },
    });
    await accept(env, { ...message, provider: "linear", stage: "start" }, 1);
    expect(names).toEqual(["linear"]);
    expect(sessions.calls.map((call) => call.method)).toEqual(["accept"]);
  });

  test("a Linear prompt does not acknowledge, because the session is already open", async () => {
    const sessions = new FakeSessions();
    const acks: string[] = [];
    const replies = {
      acknowledge: async (sessionId: string) => {
        acks.push(sessionId);
        return { kind: "sent" as const, activityId: null };
      },
      answer: async () => ({ kind: "sent" as const, activityId: null }),
    };
    await accept(
      envWith(sessions),
      { ...message, provider: "linear", stage: "prompt" },
      1,
      replies,
    );
    expect(acks).toEqual([]);
    expect(sessions.calls.map((call) => call.method)).toEqual(["accept"]);
  });

  test("a Slack delivery never touches Linear", async () => {
    const sessions = new FakeSessions();
    const acks: string[] = [];
    const names: string[] = [];
    const replies = {
      acknowledge: async (sessionId: string) => {
        acks.push(sessionId);
        return { kind: "sent" as const, activityId: null };
      },
      answer: async () => ({ kind: "sent" as const, activityId: null }),
    };
    await accept(
      envWith(sessions, ["session-1"], {
        LINEAR_OAUTH: {
          getByName: (name: string) => {
            names.push(name);
            return {
              status: async () => ({
                accessToken: "at",
                refreshToken: "rt",
                expiresIn: 1,
                issuedAt: 0,
                scope: "",
                organizationId: null,
              }),
            };
          },
        },
      }),
      { ...message, stage: "start" },
      1,
      replies,
    );
    expect(acks).toEqual([]);
    expect(names).toEqual([]);
  });

  test("a message reaches the session with the indexed session id", async () => {
    const sessions = new FakeSessions();
    await accept(envWith(sessions), message, 1234);
    expect(sessions.calls).toEqual([
      {
        method: "accept",
        args: ["session-1", { provider: "slack", ...message.thread }, "hello", "Ev1", 1234],
      },
    ]);
  });

  test("a second delivery for the same thread uses the same session", async () => {
    const sessions = new FakeSessions();
    const env = envWith(sessions);
    await accept(env, message, 1);
    await accept(env, { ...message, eventId: "Ev2" }, 2);
    const ids = sessions.calls.map((call) => (call.args as string[])[0]);
    expect(ids).toEqual(["session-1", "session-1"]);
  });

  test("a cancel transitions the active turn", async () => {
    const sessions = new FakeSessions();
    sessions.active = { id: "t2", status: "running" } as Turn;
    await accept(
      envWith(sessions),
      {
        kind: "cancel",
        provider: "slack",
        eventId: "Ev3",
        thread: message.thread,
        authorId: "U",
      },
      99,
    );
    expect(sessions.calls).toEqual([
      { method: "active", args: ["session-1"] },
      { method: "transition", args: ["session-1", "t2", { status: "cancelled" }, 99] },
    ]);
  });

  test("a cancel with no active turn is refused and never transitions", async () => {
    const sessions = new FakeSessions();
    const outcome = await accept(
      envWith(sessions),
      {
        kind: "cancel",
        provider: "discord",
        eventId: "Ev4",
        thread: message.thread,
        authorId: "U",
      },
      1,
    );
    expect(outcome).toEqual({ kind: "refused", reason: "no-active-turn" });
    expect(sessions.calls.map((call) => call.method)).toEqual(["active"]);
  });

  test("the thread the index sees carries the provider", async () => {
    const sessions = new FakeSessions();
    const seen: ThreadRef[] = [];
    const env = envWith(sessions);
    env.THREAD_INDEX = {
      getByName: () => ({
        lookup: (thread: ThreadRef) => {
          seen.push(thread);
          return Promise.resolve("session-1");
        },
      }),
    };
    await accept(env, { ...message, provider: "linear" }, 1);
    expect(seen[0]).toEqual({ provider: "linear", ...message.thread });
  });
});

describe("the reply a settled run sends", () => {
  const thread = { provider: "linear" as const, workspaceId: "w", channelId: "i", threadId: "t" };

  /** A sink that records what the session object asked it to answer. */
  function sink() {
    const sent: { kind: string; text?: string; error?: string; threadId: string }[] = [];
    return {
      sent,
      replies: async (
        outcome: { kind: "answered"; text: string } | { kind: "failed"; error: string },
        threadId: string,
      ) => {
        sent.push({ ...outcome, threadId });
      },
    };
  }

  test("an answered run sends the agent's words to the thread", async () => {
    const { sent, replies } = sink();
    const object = new SessionObject(
      new SessionHost(),
      undefined,
      { run: async () => ({ kind: "answered", text: "the answer" }) },
      () => undefined,
      replies,
    );
    await object.accept("s1", thread, "hello", "evt-1", 10);
    await until(() => sent.length > 0);
    expect(sent).toEqual([{ kind: "answered", text: "the answer", threadId: "t" }]);
  });

  test("a failed run sends its reason, never silence", async () => {
    const { sent, replies } = sink();
    const object = new SessionObject(
      new SessionHost(),
      undefined,
      { run: async () => ({ kind: "unanswered", reason: "no key" }) },
      () => undefined,
      replies,
    );
    await object.accept("s1", thread, "hello", "evt-1", 10);
    await until(() => sent.length > 0);
    expect(sent).toEqual([{ kind: "failed", error: "no key", threadId: "t" }]);
  });

  test("a run that never settles sends no reply, so a deployment without a runner stays quiet", async () => {
    const { sent, replies } = sink();
    const object = new SessionObject(
      new SessionHost(),
      undefined,
      undefined,
      () => undefined,
      replies,
    );
    await object.accept("s1", thread, "hello", "evt-1", 10);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(sent).toEqual([]);
  });

  test("the reply names the session's thread, not the issue", async () => {
    const { sent, replies } = sink();
    const object = new SessionObject(
      new SessionHost(),
      undefined,
      { run: async () => ({ kind: "answered", text: "x" }) },
      () => undefined,
      replies,
    );
    await object.accept(
      "s1",
      { provider: "linear", workspaceId: "w", channelId: "issue-1", threadId: "sess-1" },
      "hello",
      "evt-1",
      10,
    );
    await until(() => sent.length > 0);
    expect(sent[0]?.threadId).toBe("sess-1");
  });
});

/** The in-memory storage a session object needs, enough to record a delivery. */
class SessionHost implements StorageHost {
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

  async list<T>(options: { prefix: string }): Promise<Map<string, T>> {
    return new Map(
      [...this.values.entries()]
        .filter(([key]) => key.startsWith(options.prefix))
        .map(([key, raw]) => [key, JSON.parse(raw) as T]),
    );
  }
}

/**
 * Wait until a detached run has taken effect. `accept` returns before the run,
 * because Linear needs an answer inside five seconds.
 */
async function until(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200 && !done(); attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

describe("a stdio turn whose gateway never arrives", () => {
  const thread = { provider: "linear" as const, workspaceId: "w", channelId: "i", threadId: "t" };

  /** A session object routed to the VM, with a clock the test controls. */
  function vmObject(replies: () => void = () => undefined) {
    const host = new SessionHost();
    const handed: Promise<unknown>[] = [];
    const object = new SessionObject(
      host,
      {
        policy: {
          defaultAgent: "claude-code",
          runtimePreference: ["local-vm"],
          available: ["local-vm"],
        },
      },
      { run: async () => ({ kind: "answered", text: "never" }) },
      (work) => handed.push(work),
      async () => replies(),
    );
    return { host, object, handed };
  }

  test("the delivery leaves the turn queued, because the gateway hosts it", async () => {
    const { object, handed } = vmObject();
    await object.accept("s1", thread, "hello", "evt-1", 0);
    expect(handed).toEqual([]);
    expect((await object.turns("s1"))[0]?.status).toBe("queued");
  });

  test("the turn fails at its deadline and the thread is told why", async () => {
    let answered = 0;
    const { object } = vmObject(() => {
      answered += 1;
    });
    await object.accept("s1", thread, "hello", "evt-1", 0);
    const expired = await object.expire("s1", GATEWAY_CLAIM_DEADLINE_MS);
    expect(expired).toHaveLength(1);
    const turn = (await object.turns("s1"))[0];
    expect(turn?.status).toBe("failed");
    expect(turn?.error).toContain("no gateway claimed this turn");
    expect(answered).toBe(1);
  });

  test("the turn is left alone before its deadline", async () => {
    const { object } = vmObject();
    await object.accept("s1", thread, "hello", "evt-1", 0);
    expect(await object.expire("s1", GATEWAY_CLAIM_DEADLINE_MS - 1)).toEqual([]);
    expect((await object.turns("s1"))[0]?.status).toBe("queued");
  });

  test("a claimed turn is not failed by the sweep, so a slow agent survives", async () => {
    const { object } = vmObject();
    await object.accept("s1", thread, "hello", "evt-1", 0);
    await object.transition("s1", "t1", { status: "dispatched" }, 1);
    expect(await object.expire("s1", GATEWAY_CLAIM_DEADLINE_MS * 2)).toEqual([]);
    expect((await object.turns("s1"))[0]?.status).toBe("dispatched");
  });

  test("a second sweep finds nothing, so the failure is reported once", async () => {
    let answered = 0;
    const { object } = vmObject(() => {
      answered += 1;
    });
    await object.accept("s1", thread, "hello", "evt-1", 0);
    await object.expire("s1", GATEWAY_CLAIM_DEADLINE_MS);
    expect(await object.expire("s1", GATEWAY_CLAIM_DEADLINE_MS * 2)).toEqual([]);
    expect(answered).toBe(1);
  });
});
