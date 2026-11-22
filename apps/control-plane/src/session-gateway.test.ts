import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { replyOf, SessionObject } from "./session-do.ts";
import type { TurnResult } from "./turn-runner.ts";
import { DiskHost } from "./disk-host.ts";
import { routingPolicyOf } from "./routing.ts";
import { GATEWAY_CLAIM_DEADLINE_MS, NO_GATEWAY_REASON } from "@agent/store";

/** Wait for a detached run to settle, which the object does not await. */
async function settle(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (done()) return;
    await Bun.sleep(5);
  }
  throw new Error("the detached run never settled");
}

/**
 * The deployed object, driven through its gateway surface.
 *
 * The platform class only adapts a WebSocket to `SessionObject.gateway`, so
 * what this file exercises is the path a real VM gateway reaches: a hello that
 * dispatches a queued turn, a bind that records the ACP session, a settle that
 * answers the thread, and a queued turn that no gateway claims.
 */

const SESSION = "linear:org:sess:sess";

const thread = {
  provider: "linear" as const,
  workspaceId: "org",
  channelId: "i",
  threadId: "sess",
};

/** A stdio deployment: a Linear mention is routed to a local-VM gateway. */
const vmPolicy = () =>
  routingPolicyOf({
    AGENT_DEFAULT: "claude-code",
    AGENT_RUNTIMES: "local-vm",
    AGENT_AVAILABLE_RUNTIMES: "local-vm",
  });

/** The replies the object sent, in order. */
type Reply =
  | { kind: "answered"; text: string; replyTo: string }
  | { kind: "failed"; error: string; replyTo: string };

const objectOver = async () => {
  const dir = await mkdtemp(join(tmpdir(), "agent-gateway-do-"));
  const replies: Reply[] = [];
  const object = new SessionObject(
    new DiskHost(join(dir, "sessions")),
    { policy: vmPolicy() },
    undefined,
    undefined,
    async (outcome, replyTo) => {
      replies.push({ ...outcome, replyTo });
    },
  );
  return { dir, object, replies };
};

/** A socket as the platform class supplies it. */
const socket = () => {
  const sent: string[] = [];
  return {
    sent,
    sessionId: undefined as string | undefined,
    send(frame: string) {
      sent.push(frame);
    },
  };
};

/** The turn a test means, by id: storage order says nothing about turn order. */
const turnById = async (object: SessionObject, turnId: string) =>
  (await object.turns(SESSION)).find((turn) => turn.id === turnId);

async function queued(object: SessionObject, now = 1_000): Promise<void> {
  const outcome = (await object.accept(SESSION, thread, "hello", `evt-${now}`, now)) as {
    kind: string;
  };
  if (outcome.kind !== "accepted") throw new Error(`expected a delivery, got ${outcome.kind}`);
}

describe("a gateway serving the session", () => {
  test("a hello dispatches the queued turn and claims it", async () => {
    const { dir, object } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      expect(gateway.sessionId).toBe(SESSION);
      const frame = JSON.parse(gateway.sent[0]!) as { params: Record<string, unknown> };
      expect(frame.params).toMatchObject({ type: "prompt", turnId: "t1", text: "hello" });
      expect((await turnById(object, "t1"))?.status).toBe("dispatched");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a bind records the ACP session on the stored session", async () => {
    const { dir, object } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      await object.gateway(
        gateway,
        JSON.stringify({ type: "session.bound", sessionId: SESSION, acpSessionId: "acp-7" }),
        2_001,
      );
      expect((await object.session(SESSION))?.acpSessionId).toBe("acp-7");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a completed turn is stored and answered on the session's thread", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      // A real gateway reports the start before the completion; the state table
      // only completes from `running`.
      await object.gateway(
        gateway,
        JSON.stringify({ type: "turn.started", sessionId: SESSION, turnId: "t1" }),
        2_050,
      );
      await object.gateway(
        gateway,
        JSON.stringify({
          type: "turn.completed",
          sessionId: SESSION,
          turnId: "t1",
          stopReason: "end_turn",
          text: "the agent's answer",
        }),
        2_100,
      );
      expect(await turnById(object, "t1")).toMatchObject({
        status: "completed",
        stopReason: "end_turn",
      });
      expect(replies).toEqual([{ kind: "answered", text: "the agent's answer", replyTo: "sess" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a failed turn is stored and answered with its reason", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      await object.gateway(
        gateway,
        JSON.stringify({
          type: "turn.failed",
          sessionId: SESSION,
          turnId: "t1",
          error: "the agent exited",
        }),
        2_100,
      );
      expect(await turnById(object, "t1")).toMatchObject({
        status: "failed",
        error: "the agent exited",
      });
      expect(replies).toEqual([{ kind: "failed", error: "the agent exited", replyTo: "sess" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a gateway that only speaks does not answer the thread", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a connection with no session expires nothing, so an anonymous ping is harmless", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "ping" }), 99_000_000);
      // The turn is still queued and unanswered: no session was named, so the
      // sweep had nothing to look at.
      expect((await turnById(object, "t1"))?.status).toBe("queued");
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a delivery the router refuses", () => {
  test("a refused delivery is returned as it is, with nothing recorded", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-gateway-refused-"));
    try {
      const object = new SessionObject(new DiskHost(join(dir, "sessions")), {
        // Codex runs only in a sandbox, and this deployment offers only the VM,
        // so there is no route for the delivery.
        policy: routingPolicyOf({
          AGENT_DEFAULT: "codex",
          AGENT_RUNTIMES: "local-vm",
          AGENT_AVAILABLE_RUNTIMES: "local-vm",
        }),
      });
      const outcome = (await object.accept(SESSION, thread, "hello", "evt-1", 1_000)) as {
        kind: string;
      };
      expect(outcome.kind).toBe("refused");
      // The session record exists so a later delivery can route, but no turn was
      // accepted: a refused delivery must not leave work queued for no gateway.
      expect((await object.session(SESSION))?.selection).toBeNull();
      expect(await object.turns(SESSION)).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a durable route with no runner", () => {
  test("a durable delivery with no runner is recorded and left queued rather than half-run", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-gateway-durable-"));
    try {
      // A durable route is this process, but with no runner there is nothing to
      // run it with, so the turn must stay queued for a host that can.
      const object = new SessionObject(new DiskHost(join(dir, "sessions")), {
        policy: routingPolicyOf({}),
      });
      const outcome = (await object.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-1",
        1_000,
      )) as { kind: string };
      expect(outcome.kind).toBe("accepted");
      expect((await object.turns(SESSION))[0]?.status).toBe("queued");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a durable run answers the thread from inside the run", () => {
  test("a completed run sends the agent's words, and a failed one sends the reason", async () => {
    const replies: Reply[] = [];
    const dir = await mkdtemp(join(tmpdir(), "agent-gateway-run-"));
    try {
      const answered = new SessionObject(
        new DiskHost(join(dir, "answered")),
        { policy: routingPolicyOf({}) },
        { run: async () => ({ kind: "answered" as const, text: "from pi" }) },
        undefined,
        async (outcome, replyTo) => {
          replies.push(
            outcome.kind === "answered"
              ? { kind: "answered", text: outcome.text, replyTo }
              : { kind: "failed", error: outcome.error, replyTo },
          );
        },
      );
      await answered.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-1",
        1_000,
      );
      await settle(() => replies.length > 0);
      expect(replies).toEqual([{ kind: "answered", text: "from pi", replyTo: "1" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a failed run answers with its reason, so the thread is never silent", async () => {
    const replies: Reply[] = [];
    const dir = await mkdtemp(join(tmpdir(), "agent-gateway-run-failed-"));
    try {
      const failed = new SessionObject(
        new DiskHost(join(dir, "failed")),
        { policy: routingPolicyOf({}) },
        { run: async () => ({ kind: "unanswered" as const, reason: "no model" }) },
        undefined,
        async (outcome, replyTo) => {
          replies.push(
            outcome.kind === "answered"
              ? { kind: "answered", text: outcome.text, replyTo }
              : { kind: "failed", error: outcome.error, replyTo },
          );
        },
      );
      await failed.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-1",
        1_000,
      );
      await settle(() => replies.length > 0);
      expect(replies).toEqual([{ kind: "failed", error: "no model", replyTo: "1" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a redelivery of the same event sends no second reply, because nothing is queued", async () => {
    const replies: Reply[] = [];
    const dir = await mkdtemp(join(tmpdir(), "agent-gateway-run-cancelled-"));
    try {
      const cancelled = new SessionObject(
        new DiskHost(join(dir, "cancelled")),
        { policy: routingPolicyOf({}) },
        { run: async () => ({ kind: "answered" as const, text: "first" }) },
        undefined,
        async (outcome, replyTo) => {
          replies.push(
            outcome.kind === "answered"
              ? { kind: "answered", text: outcome.text, replyTo }
              : { kind: "failed", error: outcome.error, replyTo },
          );
        },
      );
      await cancelled.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-1",
        1_000,
      );
      await settle(() => replies.length > 0);
      // The same event again is a replay, so there is no queued turn to run and
      // no second reply: a redelivery is not a new answer.
      await cancelled.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-1",
        2_000,
      );
      await Bun.sleep(30);
      expect(replies).toEqual([{ kind: "answered", text: "first", replyTo: "1" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a queued turn no gateway claims", () => {
  test("the sweep fails it with a reason that names the missing gateway and answers the thread", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      const expired = (await object.expire(
        SESSION,
        1_000 + GATEWAY_CLAIM_DEADLINE_MS,
      )) as unknown[];
      expect(expired).toHaveLength(1);
      expect(await turnById(object, "t1")).toMatchObject({
        status: "failed",
        error: NO_GATEWAY_REASON,
      });
      expect(replies).toEqual([{ kind: "failed", error: NO_GATEWAY_REASON, replyTo: "sess" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a claimed turn is left alone by the same sweep, so a slow agent is not failed", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 1_001);
      const expired = (await object.expire(
        SESSION,
        1_000 + GATEWAY_CLAIM_DEADLINE_MS,
      )) as unknown[];
      expect(expired).toEqual([]);
      expect((await object.turns(SESSION))[0]?.status).toBe("dispatched");
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a delivery is also the moment a stuck turn is reported, so the thread is never silent", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      // A second delivery a deadline later: the first turn never got a gateway.
      await queued(object, 1_000 + GATEWAY_CLAIM_DEADLINE_MS);
      expect(await turnById(object, "t1")).toMatchObject({
        status: "failed",
        error: NO_GATEWAY_REASON,
      });
      expect(await turnById(object, "t2")).toMatchObject({ status: "queued" });
      expect(replies.map((reply) => reply.kind)).toEqual(["failed"]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("the reason a turn carries is what the thread is told, when it has one", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      await object.transition(SESSION, "t1", { status: "dispatched" }, 1_001);
      await object.transition(
        SESSION,
        "t1",
        { status: "failed", error: "the agent reported a failure" },
        1_002,
      );
      expect(replies).toEqual([]);
      const expired = (await object.expire(
        SESSION,
        1_000 + GATEWAY_CLAIM_DEADLINE_MS,
      )) as unknown[];
      expect(expired).toEqual([]);
      expect(await turnById(object, "t1")).toMatchObject({
        status: "failed",
        error: "the agent reported a failure",
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a model the agent does not offer", () => {
  const slack = { provider: "slack" as const, workspaceId: "T", channelId: "C", threadId: "1" };

  /** A durable deployment whose runner records the model it was asked for. */
  const durableOver = async (name: string) => {
    const dir = await mkdtemp(join(tmpdir(), name));
    const runs: string[] = [];
    const replies: Reply[] = [];
    const object = new SessionObject(
      new DiskHost(join(dir, "sessions")),
      { policy: routingPolicyOf({}) },
      {
        run: async (input) => {
          runs.push(input.model.modelId);
          return { kind: "answered" as const, text: "ran" };
        },
      },
      undefined,
      async (outcome, replyTo) => {
        replies.push({ ...outcome, replyTo });
      },
    );
    return { dir, object, runs, replies };
  };

  /** Deliver, then let the gateway claim the turn, which is when it runs. */
  const runOneTurn = async (object: SessionObject) => {
    await object.accept(SESSION, slack, "hello", "evt-1", 1_000);
    await object.gateway(
      { sessionId: undefined, send: () => undefined },
      JSON.stringify({ type: "hello", sessionId: SESSION }),
      2_000,
    );
  };

  test("a turn whose routed model the agent does not list fails naming both", async () => {
    const { dir, object, runs, replies } = await durableOver("agent-model-miss-");
    try {
      // A real gateway reports the capability after the prompt reaches it and
      // before the agent runs the turn, so the route is decided first.
      await object.accept(SESSION, slack, "hello", "evt-1", 1_000);
      await object.capability(
        SESSION,
        { availableModels: ["only-this"], statesModels: true },
        1_001,
      );
      await object.gateway(
        { sessionId: undefined, send: () => undefined },
        JSON.stringify({ type: "hello", sessionId: SESSION }),
        2_000,
      );
      await settle(() => replies.length > 0);
      const turn = await turnById(object, "t1");
      expect(turn).toMatchObject({ status: "failed" });
      expect(turn?.error).toContain("does not offer");
      expect(turn?.error).toContain("only-this");
      // The runner never saw the turn, so the agent was not asked to run a
      // model it cannot.
      expect(runs).toEqual([]);
      expect(replies[0]?.kind).toBe("failed");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a turn whose routed model the agent lists runs", async () => {
    const { dir, object, runs } = await durableOver("agent-model-hit-");
    try {
      await object.accept(SESSION, slack, "hello", "evt-1", 1_000);
      // The route is decided by the delivery; the capability names that model.
      const routed = (await object.session(SESSION))?.selection?.model.modelId ?? "";
      await object.capability(SESSION, { availableModels: [routed], statesModels: true }, 1_001);
      await object.gateway(
        { sessionId: undefined, send: () => undefined },
        JSON.stringify({ type: "hello", sessionId: SESSION }),
        2_000,
      );
      await settle(() => runs.length > 0);
      expect(runs).toEqual([routed]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("an agent that states no models still runs, because its silence is not a refusal", async () => {
    const { dir, object, runs } = await durableOver("agent-model-silent-");
    try {
      await runOneTurn(object);
      await settle(() => runs.length > 0);
      expect(runs).toHaveLength(1);
      expect(await turnById(object, "t1")).toMatchObject({ status: "completed" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the reply a run's outcome earns", () => {
  test("an answered run replies with the agent's words", () => {
    expect(replyOf({ kind: "answered", text: "the answer" })).toEqual({
      kind: "answered",
      text: "the answer",
    });
  });

  test("a failed run replies with the reason, so the thread is never silent", () => {
    expect(replyOf({ kind: "failed", error: "no model" })).toEqual({
      kind: "failed",
      error: "no model",
    });
  });

  test("a turn that never ran earns no reply, because there is nothing to say", () => {
    expect(replyOf({ kind: "not-queued", status: "completed" })).toBeUndefined();
    expect(replyOf({ kind: "missing" })).toBeUndefined();
  });
});

describe("the object's own surface", () => {
  test("a capability recorded through the object reaches the stored session", async () => {
    const { dir, object } = await objectOver();
    try {
      await queued(object);
      const updated = (await object.capability(
        SESSION,
        { availableModels: ["a"], statesModels: true },
        5,
      )) as { capability: { availableModels: string[] } };
      expect(updated.capability.availableModels).toEqual(["a"]);
      expect((await object.session(SESSION))?.capability).toEqual({
        availableModels: ["a"],
        statesModels: true,
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a gateway connection with no session named sweeps nothing", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      // No session has been bound, so there is no id to sweep and no reply.
      await object.gateway(
        { sessionId: undefined, send: () => undefined },
        JSON.stringify({ type: "ping" }),
        99_000_000,
      );
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a stdio delivery with no runner", () => {
  test("the turn is left for the gateway, which is the only host of that agent", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-stdio-no-runner-"));
    try {
      const object = new SessionObject(new DiskHost(join(dir, "sessions")), {
        policy: vmPolicy(),
      });
      const outcome = (await object.accept(SESSION, thread, "hello", "evt-1", 1_000)) as {
        kind: string;
      };
      expect(outcome.kind).toBe("accepted");
      expect(await turnById(object, "t1")).toMatchObject({ status: "queued" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a durable run through the object", () => {
  /** A durable object whose runner answers whatever the test names. */
  const durable = async (result: TurnResult, name: string) => {
    const dir = await mkdtemp(join(tmpdir(), name));
    const replies: Reply[] = [];
    const object = new SessionObject(
      new DiskHost(join(dir, "sessions")),
      { policy: routingPolicyOf({}) },
      { run: async () => result },
      undefined,
      async (outcome, replyTo) => {
        replies.push({ ...outcome, replyTo });
      },
    );
    return { dir, object, replies };
  };

  const slack = { provider: "slack" as const, workspaceId: "T", channelId: "C", threadId: "1" };

  test("the object answers the thread after the run, not before", async () => {
    const { dir, object, replies } = await durable({ kind: "answered", text: "after" }, "obj-run-");
    try {
      await object.accept(SESSION, slack, "hello", "evt-1", 1_000);
      // The run is detached, so the webhook returns before the answer exists.
      await settle(() => replies.length > 0);
      expect(replies).toEqual([{ kind: "answered", text: "after", replyTo: "1" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a run with no queued turn answers nothing, because nothing ran", async () => {
    const { dir, object, replies } = await durable({ kind: "answered", text: "x" }, "obj-notq-");
    try {
      // No delivery means no queued turn, so the detached run finds nothing.
      await object.gateway(
        { sessionId: SESSION, send: () => undefined },
        JSON.stringify({ type: "ping" }),
        1_000,
      );
      await Bun.sleep(20);
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a run that finds nothing queued", () => {
  test("the object answers nothing, so an empty sweep cannot reply on the thread", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-nothing-queued-"));
    try {
      const replies: Reply[] = [];
      const object = new SessionObject(
        new DiskHost(join(dir, "sessions")),
        { policy: routingPolicyOf({}) },
        { run: async () => ({ kind: "answered" as const, text: "should not run" }) },
        undefined,
        async (outcome, replyTo) => {
          replies.push({ ...outcome, replyTo });
        },
      );
      // A session with no turn at all: the detached run finds nothing to claim
      // and reports nothing, which is what keeps a redelivery silent.
      const slack = { provider: "slack" as const, workspaceId: "T", channelId: "C", threadId: "1" };
      await object.accept(SESSION, slack, "hello", "evt-1", 1_000);
      const first = await object.turns(SESSION);
      expect(first).toHaveLength(1);
      await settle(() => replies.length > 0);
      const answered = replies.length;
      // A second delivery of the same event is a replay, so no new turn exists
      // and no second reply is earned.
      await object.accept(SESSION, slack, "hello", "evt-1", 2_000);
      await Bun.sleep(50);
      expect((await object.turns(SESSION)).length).toBe(1);
      expect(replies.length).toBe(answered);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a durable delivery with no runner", () => {
  test("the turn is recorded and left queued for a host that can run it", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-durable-no-runner-"));
    try {
      const object = new SessionObject(new DiskHost(join(dir, "sessions")), {
        policy: routingPolicyOf({}),
      });
      const slack = { provider: "slack" as const, workspaceId: "T", channelId: "C", threadId: "1" };
      const outcome = (await object.accept(SESSION, slack, "hello", "evt-1", 1_000)) as {
        kind: string;
      };
      expect(outcome.kind).toBe("accepted");
      // No runner means no in-process run, so nothing claims the turn.
      expect(await turnById(object, "t1")).toMatchObject({ status: "queued" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("expiring a turn on a session that is gone", () => {
  test("a sweep for a session with no record answers nothing and replies nothing", async () => {
    const dir = await mkdtemp(join(tmpdir(), "agent-expire-missing-"));
    try {
      const replies: Reply[] = [];
      const object = new SessionObject(
        new DiskHost(join(dir, "sessions")),
        { policy: routingPolicyOf({}) },
        undefined,
        undefined,
        async (outcome, replyTo) => {
          replies.push({ ...outcome, replyTo });
        },
      );
      expect(await object.expire(SESSION, 1_000_000)).toEqual([]);
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the reply a gateway's settle earns", () => {
  test("a failed settle answers with the error it carried, never a text", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(gateway, JSON.stringify({ type: "hello", sessionId: SESSION }), 2_000);
      await object.gateway(
        gateway,
        JSON.stringify({ type: "turn.failed", sessionId: SESSION, turnId: "t1", error: "died" }),
        2_100,
      );
      expect(replies).toEqual([{ kind: "failed", error: "died", replyTo: "sess" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a capability reaching the object through the gateway", () => {
  test("the object keeps the capability the agent reported, so a later turn is checked", async () => {
    const { dir, object } = await objectOver();
    try {
      await queued(object);
      const gateway = socket();
      await object.gateway(
        gateway,
        JSON.stringify({
          type: "session.capability",
          sessionId: SESSION,
          capability: { availableModels: ["agent-only"], statesModels: true },
        }),
        2_000,
      );
      expect((await object.session(SESSION))?.capability).toEqual({
        availableModels: ["agent-only"],
        statesModels: true,
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a detached run that finds nothing", () => {
  test("it logs no failure, because a turn that was never queued is not an error", async () => {
    const logged: string[] = [];
    const original = console.log;
    console.log = (message: string) => logged.push(String(message));
    const dir = await mkdtemp(join(tmpdir(), "agent-nothing-run-"));
    try {
      const object = new SessionObject(
        new DiskHost(join(dir, "sessions")),
        { policy: routingPolicyOf({}) },
        { run: async () => ({ kind: "answered" as const, text: "never" }) },
      );
      // A gateway speaking for a session with no queued turn starts no run.
      await object.gateway(
        { sessionId: SESSION, send: () => undefined },
        JSON.stringify({ type: "ping" }),
        1_000,
      );
      await Bun.sleep(30);
      expect(logged.filter((line) => line.includes("turn run failed"))).toEqual([]);
    } finally {
      console.log = original;
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a gateway that never named a session", () => {
  test("the sweep is skipped, so an anonymous connection expires nothing", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      // A connection with no hello has no session to sweep, and the turn is far
      // past its deadline; naming "" must find nothing rather than throw.
      await object.gateway(
        { sessionId: undefined, send: () => undefined },
        JSON.stringify({ type: "ping" }),
        1_000 + GATEWAY_CLAIM_DEADLINE_MS * 2,
      );
      expect(replies).toEqual([]);
      expect((await turnById(object, "t1"))?.status).toBe("queued");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a sweep that finds nothing", () => {
  test("no session read is needed, so a missing record is not the reason nothing replied", async () => {
    // The sweep short-circuits on an empty result, which is what keeps a
    // delivery to a session with no stale turn from reading storage again.
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      // Inside the deadline, so nothing expires.
      expect(await object.expire(SESSION, 1_001)).toEqual([]);
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("an expired turn with no recorded error is reported with the gateway reason", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      const expired = (await object.expire(SESSION, 1_000 + GATEWAY_CLAIM_DEADLINE_MS)) as Array<{
        error: string | null;
      }>;
      expect(expired).toHaveLength(1);
      // The turn carries the reason from the sweep, so the reply names it.
      expect(replies[0]).toEqual({
        kind: "failed",
        error: NO_GATEWAY_REASON,
        replyTo: "sess",
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a capability on a session with no route", () => {
  test("the route is what a model check needs, so a session without one refuses nothing", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      // No delivery, so the session has a record but no route. A capability
      // arriving now has nothing to check it against.
      await object.gateway(
        { sessionId: undefined, send: () => undefined },
        JSON.stringify({ type: "hello", sessionId: SESSION }),
        1_000,
      );
      await object.gateway(
        { sessionId: SESSION, send: () => undefined },
        JSON.stringify({
          type: "session.capability",
          sessionId: SESSION,
          capability: { availableModels: ["a"], statesModels: true },
        }),
        1_001,
      );
      expect(replies).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a gateway message on a bound connection", () => {
  test("the connection's session is swept, so a stale turn fails on the next message", async () => {
    const { dir, object, replies } = await objectOver();
    try {
      await queued(object, 1_000);
      // A message long after the deadline, on a connection that knows its
      // session: the sweep must use that id, not an empty one.
      await object.gateway(
        { sessionId: SESSION, send: () => undefined },
        JSON.stringify({ type: "ping" }),
        1_000 + GATEWAY_CLAIM_DEADLINE_MS,
      );
      expect(await turnById(object, "t1")).toMatchObject({
        status: "failed",
        error: NO_GATEWAY_REASON,
      });
      expect(replies).toEqual([{ kind: "failed", error: NO_GATEWAY_REASON, replyTo: "sess" }]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
