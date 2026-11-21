import { describe, expect, test } from "bun:test";
import { accept } from "./worker.ts";
import type { Env } from "./worker.ts";
import type { ThreadRef, Turn } from "@agent/store";

/**
 * A session object that records what the Worker asked it to do, standing in
 * for the Durable Object across the RPC boundary.
 */
class FakeSessions {
  readonly calls: Array<{ method: string; args: unknown[] }> = [];
  active: Turn | undefined;

  active_(sessionId: string): Promise<Turn | undefined> {
    this.calls.push({ method: "active", args: [sessionId] });
    return Promise.resolve(this.active);
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

const envWith = (sessions: FakeSessions, sessionIds: string[] = ["session-1"]): Env => {
  const lookups: ThreadRef[] = [];
  const env: Env = {
    slack: { signingSecret: "s", botUserId: "B" },
    discord: { publicKey: "p" },
    linear: { webhookSecret: "l" },
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
        deliver: (...args: unknown[]) => sessions.deliver(...args),
        transition: (...args: unknown[]) => sessions.transition(...args),
      }),
    },
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
};

describe("the control plane to session relay", () => {
  test("a message reaches deliver with the indexed session id", async () => {
    const sessions = new FakeSessions();
    await accept(envWith(sessions), message, 1234);
    expect(sessions.calls).toEqual([
      {
        method: "deliver",
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
