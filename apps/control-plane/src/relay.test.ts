import { describe, expect, test } from "bun:test";
import { accept } from "./worker.ts";
import type { Env } from "./worker.ts";
import type { ThreadRef, Turn } from "@agent/store";
import { OAUTH } from "./test-env.ts";
import { SessionObject } from "./session-do.ts";
import type { StorageHost } from "./storage.ts";

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

describe("answering a Linear session with the agent's words", () => {
  const port = (sent: { sessionId: string; text: string }[]) => ({
    acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
    answer: async (sessionId: string, text: string) => {
      sent.push({ sessionId, text });
      return { kind: "sent" as const, activityId: "a-1" };
    },
  });

  test("an answered turn reaches the session's thread", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = { kind: "answered", text: "the build is green" };
    const sent: { sessionId: string; text: string }[] = [];
    const env = envWith(sessions, ["session-1"]);
    const outcome = (await accept(
      env,
      { ...message, provider: "linear", stage: "start" },
      1,
      port(sent),
    )) as { reply?: unknown };
    expect(sent).toEqual([{ sessionId: message.thread.threadId, text: "the build is green" }]);
    expect(outcome.reply).toEqual({ kind: "sent", activityId: "a-1" });
  });

  test("a failed turn says so rather than leaving the session silent", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = { kind: "failed", error: "no model key" };
    const sent: { sessionId: string; text: string }[] = [];
    const env = envWith(sessions, ["session-1"]);
    await accept(env, { ...message, provider: "linear", stage: "start" }, 1, port(sent));
    expect(sent[0]?.text).toContain("no model key");
  });

  test("a turn that never ran sends no answer, so a runner-less deployment stays quiet", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = undefined;
    const sent: { sessionId: string; text: string }[] = [];
    const env = envWith(sessions, ["session-1"]);
    const outcome = (await accept(
      env,
      { ...message, provider: "linear", stage: "start" },
      1,
      port(sent),
    )) as { reply?: unknown };
    expect(sent).toEqual([]);
    expect(outcome.reply).toBeUndefined();
  });

  test("a Slack delivery sends no Linear answer, because it has no Linear session", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = { kind: "answered", text: "hello" };
    const sent: { sessionId: string; text: string }[] = [];
    const env = envWith(sessions, ["session-1"]);
    await accept(env, message, 1, port(sent));
    expect(sent).toEqual([]);
  });

  test("a refused delivery sends no answer", async () => {
    const sessions = new FakeSessions();
    sessions.refuse = true;
    const sent: { sessionId: string; text: string }[] = [];
    const env = envWith(sessions, ["session-1"]);
    await accept(env, { ...message, provider: "linear", stage: "start" }, 1, port(sent));
    expect(sent).toEqual([]);
  });
});

describe("the answer a run produces", () => {
  const port = (sent: string[]) => ({
    acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
    answer: async (_sessionId: string, text: string) => {
      sent.push(text);
      return { kind: "sent" as const, activityId: null };
    },
  });

  const runWith = async (run: { kind: string; text?: string; error?: string } | undefined) => {
    const sessions = new FakeSessions();
    sessions.answerWith = run as never;
    const sent: string[] = [];
    await accept(
      envWith(sessions, ["session-1"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port(sent),
    );
    return sent;
  };

  test("an answered run sends its text", async () => {
    expect(await runWith({ kind: "answered", text: "done" })).toEqual(["done"]);
  });

  test("an answered run with no text sends an empty answer rather than nothing", async () => {
    expect(await runWith({ kind: "answered" })).toEqual([""]);
  });

  test("a failed run sends its reason", async () => {
    expect(await runWith({ kind: "failed", error: "boom" })).toEqual(["The run failed: boom"]);
  });

  test("a failed run with no reason still says it failed", async () => {
    expect(await runWith({ kind: "failed" })).toEqual(["The run failed: no reason given"]);
  });

  test("a run in an unrecognized state sends nothing", async () => {
    expect(await runWith({ kind: "not-queued" })).toEqual([]);
  });
});

describe("the outcome a Linear delivery returns", () => {
  const port = (sent: string[]) => ({
    acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
    answer: async (_sessionId: string, text: string) => {
      sent.push(text);
      return { kind: "sent" as const, activityId: null };
    },
  });

  test("a refused delivery returns the refusal, so the provider can retry", async () => {
    const sessions = new FakeSessions();
    sessions.refuse = true;
    const outcome = await accept(
      envWith(sessions, ["session-1"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port([]),
    );
    expect(outcome).toEqual({ kind: "refused", reason: "closed" });
  });

  test("a delivery with no run reports that nothing ran", async () => {
    const sessions = new FakeSessions();
    const outcome = (await accept(
      envWith(sessions, ["session-1"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port([]),
    )) as { reply?: unknown };
    expect(outcome.reply).toBeUndefined();
  });
});

describe("a refused delivery is returned before a reply is attempted", () => {
  test("the port is never asked to answer a refused delivery", async () => {
    const sessions = new FakeSessions();
    sessions.refuse = true;
    const asked: string[] = [];
    const outcome = (await accept(
      envWith(sessions, ["session-1"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      {
        acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
        answer: async (_sessionId: string, text: string) => {
          asked.push(text);
          return { kind: "sent" as const, activityId: null };
        },
      },
    )) as { kind?: string; reply?: unknown };
    expect(asked).toEqual([]);
    expect(outcome.kind).toBe("refused");
  });
});

describe("a delivery that carries no run", () => {
  test("a delivery with no run gets no reply, and one with a run does", async () => {
    const withRun = new FakeSessions();
    withRun.answerWith = { kind: "answered", text: "yes" };
    const ran: string[] = [];
    await accept(envWith(withRun, ["s"]), { ...message, provider: "linear", stage: "start" }, 1, {
      acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
      answer: async (_id: string, text: string) => {
        ran.push(text);
        return { kind: "sent" as const, activityId: null };
      },
    });

    const withoutRun = new FakeSessions();
    const quiet: string[] = [];
    await accept(
      envWith(withoutRun, ["s"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      {
        acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
        answer: async (_id: string, text: string) => {
          quiet.push(text);
          return { kind: "sent" as const, activityId: null };
        },
      },
    );

    expect(ran).toEqual(["yes"]);
    expect(quiet).toEqual([]);
  });
});

describe("the object's accept records before it runs", () => {
  test("a runner runs the turn the delivery created and attaches its outcome", async () => {
    const host = new SessionHost();
    const object = new SessionObject(host, undefined, {
      run: async () => ({ kind: "answered", text: "ran" }),
    });
    const outcome = (await object.accept(
      "s1",
      { provider: "linear", workspaceId: "w", channelId: "i", threadId: "t" },
      "hello",
      "evt-1",
      10,
    )) as { run?: unknown; turn?: unknown };
    expect(outcome.run).toEqual({ kind: "answered", text: "ran" });
    expect(outcome.turn).toBeDefined();
  });

  test("a deployment with no runner records the turn and runs nothing", async () => {
    const object = new SessionObject(new SessionHost());
    const outcome = (await object.accept(
      "s1",
      { provider: "linear", workspaceId: "w", channelId: "i", threadId: "t" },
      "hello",
      "evt-1",
      10,
    )) as { run?: unknown };
    expect(outcome.run).toBeUndefined();
  });

  test("a refused delivery never asks the runner", async () => {
    let runs = 0;
    const closed = new SessionObject(new SessionHost(), undefined, {
      run: async () => {
        runs += 1;
        return { kind: "answered", text: "x" };
      },
    });
    const thread = { provider: "linear" as const, workspaceId: "w", channelId: "i", threadId: "t" };
    await closed.accept("s1", thread, "hello", "evt-1", 10);
    await closed.close("s1", 11);
    const outcome = (await closed.accept("s1", thread, "again", "evt-2", 12)) as {
      kind?: string;
    };
    expect(outcome.kind).toBe("refused");
    expect(runs).toBe(1);
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

describe("a refused Linear delivery sends no reply even with a live port", () => {
  test("the port is never asked, and the refusal reaches the caller", async () => {
    const sessions = new FakeSessions();
    sessions.refuse = true;
    const asked: string[] = [];
    const outcome = (await accept(
      envWith(sessions, ["session-1"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      {
        acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
        answer: async (_sessionId: string, text: string) => {
          asked.push(text);
          return { kind: "sent" as const, activityId: null };
        },
      },
    )) as { kind?: string; reply?: unknown };
    expect(asked).toEqual([]);
    expect(outcome.kind).toBe("refused");
    expect(outcome.reply).toBeUndefined();
  });
});

describe("the outcome a Linear caller sees", () => {
  const port = (sent: string[]) => ({
    acknowledge: async () => ({ kind: "sent" as const, activityId: null }),
    answer: async (_sessionId: string, text: string) => {
      sent.push(text);
      return { kind: "sent" as const, activityId: null };
    },
  });

  test("a run that produced an answer yields a reply field", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = { kind: "answered", text: "hi" };
    const outcome = (await accept(
      envWith(sessions, ["s"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port([]),
    )) as Record<string, unknown>;
    expect("reply" in outcome).toBe(true);
    expect(outcome.reply).toEqual({ kind: "sent", activityId: null });
  });

  test("a run with no answer yields no reply field at all", async () => {
    const sessions = new FakeSessions();
    sessions.answerWith = { kind: "not-queued" } as never;
    const outcome = (await accept(
      envWith(sessions, ["s"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port([]),
    )) as Record<string, unknown>;
    expect("reply" in outcome).toBe(false);
  });

  test("a refusal yields no reply field and no answer", async () => {
    const sessions = new FakeSessions();
    sessions.refuse = true;
    const sent: string[] = [];
    const outcome = (await accept(
      envWith(sessions, ["s"]),
      { ...message, provider: "linear", stage: "start" },
      1,
      port(sent),
    )) as Record<string, unknown>;
    expect("reply" in outcome).toBe(false);
    expect(sent).toEqual([]);
  });
});
