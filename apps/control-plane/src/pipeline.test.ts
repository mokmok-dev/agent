import { describe, expect, test } from "bun:test";
import { createHmac } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { frameText, onHello, onMessage } from "./gateway-connection.ts";
import { effectOf, nextPrompt, reportOf } from "./gateway-plane.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { DiskHost } from "./disk-host.ts";
import { routingPolicyOf } from "./routing.ts";
import { fetch as controlPlane, type Env, type Namespace } from "./worker.ts";
import { replyFor } from "./worker.ts";
import { linearReplies as _ } from "./linear-reply.ts";
import type { LinearOAuthPort } from "./linear-oauth-port.ts";

/**
 * The real control plane over disk storage, driven the way Linear and the VM
 * gateway drive it: a signed webhook, then the gateway's own connection
 * handlers. Nothing here is a stub except the Linear GraphQL endpoint, which
 * the reply path takes as a parameter.
 */

const SECRET = "linear-test-secret";
const SESSION = "linear:org-local:issue-local:sess-local";

/**
 * The whole step a connection produced. Asserting the shape, not one field,
 * is what pins "nothing was sent and the connection kept its session".
 */
function ignoredStep(sessionId: string | undefined): {
  send: string[];
  connection: { sessionId: string | undefined };
} {
  return { send: [], connection: { sessionId } };
}

/** The deployment's Linear authorization, with a token that never expires. */
function oauthPort(): LinearOAuthPort {
  return {
    start: async () => undefined,
    complete: async () => ({ kind: "rejected", status: 400, message: "not used" }),
    status: async () => undefined,
    accessToken: async () => ({ kind: "token", token: "local-token" }),
  };
}

/** The env a VM-routed deployment presents, over a temp state directory. */
async function deployment() {
  const dir = await mkdtemp(join(tmpdir(), "pipeline-"));
  const policy = routingPolicyOf({
    AGENT_DEFAULT: "claude-code",
    AGENT_RUNTIMES: "local-vm",
    AGENT_AVAILABLE_RUNTIMES: "local-vm",
  });
  const namespace = (build: (host: DiskHost) => unknown, name: string): Namespace => ({
    getByName: (id: string) => build(new DiskHost(join(dir, name, id))),
  });
  const env: Env = {
    SLACK_SIGNING_SECRET: "s",
    SLACK_BOT_USER_ID: "B",
    DISCORD_PUBLIC_KEY: "d",
    LINEAR_WEBHOOK_SECRET: SECRET,
    LINEAR_CLIENT_ID: "id",
    LINEAR_CLIENT_SECRET: "secret",
    LINEAR_REDIRECT_URI: "http://localhost/oauth/linear/callback",
    AGENT_DEFAULT: "claude-code",
    AGENT_RUNTIMES: "local-vm",
    AGENT_AVAILABLE_RUNTIMES: "local-vm",
    SESSIONS: namespace((host) => new SessionObject(host, { policy }), "sessions"),
    THREAD_INDEX: namespace((host) => new ThreadIndexObject(host), "index"),
    // The acknowledgement and the reply both read the deployment's one Linear
    // authorization object; a test supplies one whose token never expires.
    LINEAR_OAUTH: { getByName: () => oauthPort() },
  };
  return {
    dir,
    env,
    sessionObjects: () =>
      new SessionObject(new DiskHost(join(dir, "sessions", SESSION)), { policy }),
  };
}

/** The webhook Linear sends when it opens an agent session. */
function signedSession(body: unknown): Request {
  const text = JSON.stringify(body);
  const signature = createHmac("sha256", SECRET).update(text).digest("hex");
  return new Request("https://x/webhooks/linear", {
    method: "POST",
    headers: { "content-type": "application/json", "linear-signature": signature },
    body: text,
  });
}

const sessionEvent = (text: string, id = "wh-1") => ({
  type: "AgentSessionEvent",
  action: "created",
  organizationId: "org-local",
  webhookId: id,
  agentSession: { id: "sess-local" },
  issue: { id: "issue-local" },
  actor: { id: "user-local" },
  agentActivity: { type: "prompt", body: text },
});

describe("a Linear mention routed to the local VM", () => {
  test("the stored route names local-vm and a stdio agent, and the turn stays queued", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      const response = await controlPlane(signedSession(sessionEvent("write the note")), env);
      expect(response.status).toBe(202);

      const session = await sessionObjects().session(SESSION);
      // The whole route, so a wrong model or protocol cannot pass unnoticed.
      expect(session?.selection).toEqual({
        agent: "claude-code",
        runtime: "local-vm",
        protocol: "stdio",
        model: { provider: "anthropic", modelId: "claude-opus-4-7" },
      });

      const turn = (await sessionObjects().turns(SESSION))[0];
      // Queued, not completed: the pi loop in the Worker must not run a
      // stdio-routed turn.
      expect(turn?.status).toBe("queued");
      expect((await sessionObjects().session(SESSION))?.turnSeq).toBe(1);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a durable-routed deployment keeps the in-process loop", async () => {
    const { dir, env } = await deployment();
    try {
      // The default policy prefers the sandbox and pi is durable there.
      const sandboxEnv: Env = {
        ...env,
        SESSIONS: {
          getByName: () =>
            new SessionObject(new DiskHost(join(dir, "sandbox-sessions")), {
              policy: routingPolicyOf({}),
            }),
        },
      };
      const response = await controlPlane(signedSession(sessionEvent("hello")), sandboxEnv);
      expect(response.status).toBe(202);
      const stored = new SessionObject(new DiskHost(join(dir, "sandbox-sessions"))).session(
        SESSION,
      );
      expect((await stored)?.selection).toMatchObject({
        runtime: "cloudflare-sandbox",
        protocol: "durable",
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the gateway connection", () => {
  test("a queued turn is dispatched with the session, ACP id, turn id, and text", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("write the note")), env);
      const step = await onHello(sessionObjects(), SESSION, 1_000);
      expect(step.connection).toEqual({ sessionId: SESSION });
      // The frame is a JSON-RPC notification, because the gateway parses every
      // message with the ACP codec; `params` is the command itself.
      const envelope = JSON.parse(step.send[0] ?? "{}") as {
        method: string;
        params: Record<string, unknown>;
      };
      expect(envelope.method).toBe("gateway/command");
      // The whole params object, so no field can be silently dropped. The
      // route's model travels with the prompt, because a stdio agent picks its
      // own model unless the gateway selects one.
      expect(envelope.params).toEqual({
        type: "prompt",
        sessionId: SESSION,
        turnId: "t1",
        text: "write the note",
        acpSessionId: "",
        model: { provider: "anthropic", modelId: "claude-opus-4-7" },
      });
      // Claimed before it left, so a second connection is not handed it again.
      expect((await sessionObjects().turns(SESSION))[0]?.status).toBe("dispatched");
      const second = await onHello(sessionObjects(), SESSION, 1_001);
      expect(second.send).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("nothing is sent for a session with no queued turn", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      const step = await onHello(sessionObjects(), "no-such-session", 1);
      expect(step.send).toEqual([]);
      // The connection still learns which session it asked for, so a later
      // message is attributed to it rather than dropped.
      expect(step.connection).toEqual({ sessionId: "no-such-session" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a session with no queued turn still binds its connection to the id", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("already done")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      // Settle it, so the queue is empty but the session exists.
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_001,
      );
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.completed",
            sessionId: SESSION,
            turnId: "t1",
            stopReason: "end_turn",
            text: "x",
          },
        }),
        1_002,
      );
      const step = await onHello(sessionObjects(), SESSION, 1_003);
      expect(step.send).toEqual([]);
      expect(step.connection).toEqual({ sessionId: SESSION });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a turn the gateway completed while away is not re-sent", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("done already")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.completed",
            sessionId: SESSION,
            turnId: "t1",
            stopReason: "end_turn",
            text: "x",
          },
        }),
        1_002,
      );
      const again = await onHello(sessionObjects(), SESSION, 1_003);
      expect(again.send).toEqual([]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the gateway's events settle the turn", () => {
  test("session.bound records the ACP session id", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "session.bound", sessionId: SESSION, acpSessionId: "acp-9" },
        }),
        1_000,
      );
      expect(step.settled).toBeUndefined();
      expect((await sessionObjects().session(SESSION))?.acpSessionId).toBe("acp-9");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("turn.completed completes the turn and yields the agent's text", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_000,
      );
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.completed",
            sessionId: SESSION,
            turnId: "t1",
            stopReason: "end_turn",
            text: "the release note",
          },
        }),
        1_001,
      );
      expect(step.settled).toEqual({
        sessionId: SESSION,
        replyTo: "sess-local",
        text: "the release note",
      });
      const turn = (await sessionObjects().turns(SESSION))[0];
      expect(turn?.status).toBe("completed");
      expect(turn?.stopReason).toBe("end_turn");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("turn.failed fails the turn with the reported reason", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.failed",
            sessionId: SESSION,
            turnId: "t1",
            error: "the agent exited",
          },
        }),
        1_001,
      );
      expect(step.settled).toEqual({
        sessionId: SESSION,
        replyTo: "sess-local",
        error: "the agent exited",
      });
      const turn = (await sessionObjects().turns(SESSION))[0];
      expect(turn?.status).toBe("failed");
      expect(turn?.error).toBe("the agent exited");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a report for another session is ignored", () => {
    const effect = effectOf(
      { type: "turn.completed", sessionId: "other", turnId: "t1", stopReason: "end_turn" },
      { id: SESSION } as never,
      [],
    );
    expect(effect).toEqual({ kind: "ignore", reason: "event is for another session" });
  });
});

describe("the Linear reply", () => {
  /** A port whose token is fixed, so the reply path is what the test exercises. */
  const oauth = (): LinearOAuthPort => ({
    start: async () => undefined,
    complete: async () => ({ kind: "rejected", status: 400, message: "not used" }),
    status: async () => undefined,
    accessToken: async () => ({ kind: "token", token: "local-token" }),
  });

  /** The GraphQL call, captured, with the response Linear would send. */
  function capture(): {
    sent: { url: string; body: string }[];
    request: (url: string, init: { body: string }) => Promise<unknown>;
  } {
    const sent: { url: string; body: string }[] = [];
    return {
      sent,
      request: async (url, init) => {
        sent.push({ url, body: init.body });
        return { data: { agentActivityCreate: { success: true, agentActivity: { id: "a-1" } } } };
      },
    };
  }

  test("the activity body is the agent's text", async () => {
    const { request, sent } = capture();
    // Through the Worker's own reply helper, so the text the gateway settled
    // is what the production path sends.
    const outcome = await replyFor(
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (await import("./linear-reply.ts")).linearReplies(
        oauth(),
        request as never,
        "http://capture/graphql",
      ),
      "sess-local",
      { kind: "answered", text: "the release note" },
      1,
    );
    expect(outcome).toEqual({ kind: "sent", activityId: "a-1" });
    const input = JSON.parse(sent[0]?.body ?? "{}").variables.input;
    expect(input.content).toEqual({ type: "response", body: "the release note" });
    expect(input.agentSessionId).toBe("sess-local");
  });

  test("a failed run's body names the failure, never silence", async () => {
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    await replyFor(
      linearReplies(oauth(), request as never, "http://capture/graphql"),
      "sess-local",
      { kind: "failed", error: "the agent exited" },
      1,
    );
    const body = JSON.parse(sent[0]?.body ?? "{}").variables.input.content.body;
    expect(body).toContain("the agent exited");
  });

  test("a run with no reason still names a failure rather than staying silent", async () => {
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    await replyFor(
      linearReplies(oauth(), request as never, "http://capture/graphql"),
      "sess-local",
      { kind: "failed" },
      1,
    );
    const body = JSON.parse(sent[0]?.body ?? "{}").variables.input.content.body;
    expect(body).toBe("The run failed: no reason given");
  });

  test("a failed run's body is not mistaken for an answer", async () => {
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    await replyFor(
      linearReplies(oauth(), request as never, "http://capture/graphql"),
      "sess-local",
      { kind: "failed", error: "the agent exited", text: "stale text" },
      1,
    );
    const body = JSON.parse(sent[0]?.body ?? "{}").variables.input.content.body;
    expect(body).toBe("The run failed: the agent exited");
  });

  test("an answered run with no text sends an empty body, not a failure", async () => {
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    await replyFor(
      linearReplies(oauth(), request as never, "http://capture/graphql"),
      "sess-local",
      { kind: "answered" },
      1,
    );
    const body = JSON.parse(sent[0]?.body ?? "{}").variables.input.content.body;
    expect(body).toBe("");
  });

  test("a run of some other kind sends nothing, because it is neither an answer nor a failure", async () => {
    // A cancelled or unclaimed run must not be reported as an empty answer or a
    // failure with no reason: it simply has nothing to say.
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    for (const kind of ["cancelled", "not-queued", "missing"]) {
      expect(
        await replyFor(
          linearReplies(oauth(), request as never, "http://capture/graphql"),
          "sess-local",
          { kind, error: "carried but not a failure kind" },
          1,
        ),
      ).toBeUndefined();
    }
    expect(sent).toEqual([]);
  });

  test("a run that never produced an outcome sends nothing", async () => {
    const { request, sent } = capture();
    const { linearReplies } = await import("./linear-reply.ts");
    expect(
      await replyFor(
        linearReplies(oauth(), request as never, "http://capture/graphql"),
        "sess-local",
        undefined,
        1,
      ),
    ).toBeUndefined();
    expect(sent).toEqual([]);
  });
});

describe("the payload the gateway sends", () => {
  test("a JSON-RPC gateway/event notification is read as a report", () => {
    const report = reportOf({
      jsonrpc: "2.0",
      method: "gateway/event",
      params: {
        type: "turn.completed",
        sessionId: "s",
        turnId: "t",
        stopReason: "end_turn",
        text: "hi",
      },
    });
    expect(report).toEqual({
      type: "turn.completed",
      sessionId: "s",
      turnId: "t",
      stopReason: "end_turn",
      text: "hi",
    });
  });

  test("a bare object is read too, so a hand-written frame is valid", () => {
    expect(reportOf({ type: "session.bound", sessionId: "s", acpSessionId: "a" })).toEqual({
      type: "session.bound",
      sessionId: "s",
      acpSessionId: "a",
    });
  });

  test("a frame with no session is not a report", () => {
    expect(reportOf({ type: "turn.completed", turnId: "t" })).toBeUndefined();
  });

  test("the prompt a connection receives matches the queued turn", () => {
    const frame = nextPrompt({ id: SESSION, acpSessionId: "acp-1" } as never, [
      { id: "t2", prompt: "text", status: "queued", createdAt: 5 } as never,
    ]);
    // A session that has not been routed yet has no model to send, so the frame
    // carries the turn and nothing it cannot know.
    expect(frame).toEqual({
      type: "prompt",
      sessionId: SESSION,
      acpSessionId: "acp-1",
      turnId: "t2",
      text: "text",
    });
  });

  test("a routed session's model travels with the prompt, so the agent can select it", () => {
    const frame = nextPrompt(
      {
        id: SESSION,
        acpSessionId: "acp-1",
        selection: { model: { provider: "ollama-cloud", modelId: "deepseek-v4.1-flash" } },
      } as never,
      [{ id: "t2", prompt: "text", status: "queued", createdAt: 5 } as never],
    );
    expect(frame?.model).toEqual({ provider: "ollama-cloud", modelId: "deepseek-v4.1-flash" });
  });
});

describe("what the connection ignores", () => {
  test("a message that is not JSON leaves the connection and the send list alone", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      const step = await onMessage(sessionObjects(), { sessionId: SESSION }, "{not json", 1);
      expect(step).toEqual(ignoredStep(SESSION));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a frame that is not a report is ignored without touching storage", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "turn.update",
          params: { type: "turn.update", sessionId: SESSION },
        }),
        1_000,
      );
      expect(step).toEqual(ignoredStep(SESSION));
      expect((await sessionObjects().turns(SESSION))[0]?.status).toBe("queued");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a report for a session that does not exist moves nothing", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      const step = await onMessage(
        sessionObjects(),
        { sessionId: "missing" },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.completed", sessionId: "missing", turnId: "t1", text: "x" },
        }),
        1_000,
      );
      expect(step.send).toEqual([]);
      expect(step.settled).toBeUndefined();
      expect(step.connection).toEqual({ sessionId: "missing" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a connection with no session takes it from the report itself", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_001,
      );
      const step = await onMessage(
        sessionObjects(),
        { sessionId: undefined },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.completed",
            sessionId: SESSION,
            turnId: "t1",
            stopReason: "end_turn",
            text: "from the report",
          },
        }),
        1_002,
      );
      expect(step.connection).toEqual({ sessionId: SESSION });
      expect(step.settled).toEqual({
        sessionId: SESSION,
        replyTo: "sess-local",
        text: "from the report",
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a bind with an empty ACP id is ignored, because it would lose the binding", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "session.bound", sessionId: SESSION, acpSessionId: "acp-1" },
        }),
        1_000,
      );
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "session.bound", sessionId: SESSION, acpSessionId: "" },
        }),
        1_001,
      );
      expect((await sessionObjects().session(SESSION))?.acpSessionId).toBe("acp-1");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a started event for a turn that is not dispatched is ignored", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      // The turn is queued, not dispatched, so a start would be premature.
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_000,
      );
      expect((await sessionObjects().turns(SESSION))[0]?.status).toBe("queued");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a completion for a turn that never started is ignored rather than skipped", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.completed", sessionId: SESSION, turnId: "t1", text: "x" },
        }),
        1_001,
      );
      expect(step.settled).toBeUndefined();
      expect((await sessionObjects().turns(SESSION))[0]?.status).toBe("dispatched");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a failure for a turn that already completed is ignored", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      for (const [type, extra] of [
        ["turn.started", {}],
        ["turn.completed", { stopReason: "end_turn", text: "done" }],
      ] as const) {
        await onMessage(
          sessionObjects(),
          { sessionId: SESSION },
          JSON.stringify({
            jsonrpc: "2.0",
            method: "gateway/event",
            params: { type, sessionId: SESSION, turnId: "t1", ...extra },
          }),
          1_001,
        );
      }
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.failed", sessionId: SESSION, turnId: "t1", error: "late" },
        }),
        1_002,
      );
      expect(step.settled).toBeUndefined();
      expect((await sessionObjects().turns(SESSION))[0]?.status).toBe("completed");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("an event for a turn the session does not have is ignored", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.completed", sessionId: SESSION, turnId: "nope", text: "x" },
        }),
        1_000,
      );
      expect(step.settled).toBeUndefined();
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the exact step a connection produces", () => {
  test("a missing session yields nothing to send and keeps the asked-for id", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      expect(await onHello(sessionObjects(), "missing", 1)).toEqual(ignoredStep("missing"));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a bind yields nothing to send and no settlement", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "session.bound", sessionId: SESSION, acpSessionId: "acp-1" },
        }),
        1_000,
      );
      expect(step).toEqual(ignoredStep(SESSION));
      expect(step).not.toHaveProperty("settled");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a started event yields nothing to send and no settlement", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_001,
      );
      expect(step).toEqual(ignoredStep(SESSION));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("the connection is bound to the report's session when it had none", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: undefined },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "session.bound", sessionId: SESSION, acpSessionId: "acp-1" },
        }),
        1_000,
      );
      expect(step).toEqual(ignoredStep(SESSION));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the gateway route and the executor choice", () => {
  test("GET /gateway without an upgrade answers 426, whatever else it names", async () => {
    const { dir, env } = await deployment();
    try {
      for (const path of ["https://x/gateway", "https://x/gateway?session=s1"]) {
        const response = await controlPlane(new Request(path), env);
        expect(response.status).toBe(426);
        expect(await response.json()).toEqual({ error: "expected a WebSocket upgrade" });
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("an upgrade is refused unless it names the session the gateway serves", async () => {
    const { dir, env } = await deployment();
    try {
      for (const path of ["https://x/gateway", "https://x/gateway?session="]) {
        const response = await controlPlane(
          new Request(path, { headers: { upgrade: "websocket" } }),
          env,
        );
        expect(response.status).toBe(400);
        expect(await response.json()).toEqual({
          error: "name the session a gateway serves with ?session=<id>",
        });
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("an upgrade naming a session is handed to that session's object", async () => {
    const { dir } = await deployment();
    try {
      const asked: string[] = [];
      const object = {
        fetch: async (request: Request) => {
          asked.push(new URL(request.url).searchParams.get("session") ?? "");
          return new Response(null, { status: 101 });
        },
      };
      const env = {
        SESSIONS: { getByName: (id: string) => ({ ...object, id }) },
      } as unknown as Env;
      const response = await controlPlane(
        new Request("https://x/gateway?session=s1", {
          headers: { upgrade: "WebSocket" },
        }),
        env,
      );
      expect(response.status).toBe(101);
      // The route names the object by the session the gateway asked for, so a
      // Worker with many objects reaches the one that owns this socket.
      expect(asked).toEqual(["s1"]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test('a gateway that asks for "auto" is handed the session with a queued turn', async () => {
    const { dir, env } = await deployment();
    try {
      // A delivery creates the session and its turn, which is what a gateway
      // that connected before the delegation is waiting for.
      await controlPlane(signedSession(sessionEvent("for the gateway")), env);
      const asked: string[] = [];
      // Only the fetch the upgrade uses is replaced, so the object still
      // answers `next` and the route can tell which session has work.
      const real = env.SESSIONS;
      const withAuto = {
        ...env,
        SESSIONS: {
          getByName: (id: string) => {
            const object = real.getByName(id) as {
              next(sessionId: string): Promise<unknown>;
            };
            return {
              next: (sessionId: string) => object.next(sessionId),
              fetch: async () => {
                asked.push(id);
                return new Response(null, { status: 101 });
              },
            };
          },
        },
      } as Env;
      const response = await controlPlane(
        new Request("https://x/gateway?session=auto", { headers: { upgrade: "websocket" } }),
        withAuto,
      );
      expect(response.status).toBe(101);
      // The id is the session the delivery created, not the literal "auto".
      expect(asked).toEqual([SESSION]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("the handoff carries the resolved id on the request the object receives", async () => {
    const { dir, env } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("for the gateway")), env);
      const probe: { seen?: string | null; received?: Request } = {};
      const withProbe = {
        ...env,
        SESSIONS: {
          getByName: (id: string) => {
            const real = env.SESSIONS.getByName(id) as {
              next(sessionId: string): Promise<unknown>;
            };
            return {
              next: (sessionId: string) => real.next(sessionId),
              fetch: async (request: Request) => {
                probe.received = request;
                probe.seen = request.headers.get("x-agent-session");
                return new Response(null, { status: 101 });
              },
            };
          },
        },
      } as Env;
      const response = await controlPlane(
        new Request("https://x/gateway?session=auto", {
          headers: { upgrade: "websocket", "x-client": "kept" },
        }),
        withProbe,
      );
      expect(response.status).toBe(101);
      // The object is told which session it is, and the original headers survive
      // the handoff because the upgrade depends on them.
      expect(probe.seen).toBe(SESSION);
      expect(probe.received?.headers.get("x-client")).toBe("kept");
      expect(probe.received?.headers.get("upgrade")).toBe("websocket");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test('a gateway that asks for "auto" with nothing queued is told to delegate first', async () => {
    const { dir, env } = await deployment();
    try {
      const response = await controlPlane(
        new Request("https://x/gateway?session=auto", { headers: { upgrade: "websocket" } }),
        env,
      );
      expect(response.status).toBe(404);
      expect(await response.json()).toEqual({
        error: "no session is waiting for a gateway; delegate an issue first",
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a session that is already claimed is not offered to a second gateway", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("only once")), env);
      // The first gateway takes the turn, so it is no longer queued.
      const step = await onHello(sessionObjects(), SESSION, 1_000);
      expect(step.send).toHaveLength(1);
      const response = await controlPlane(
        new Request("https://x/gateway?session=auto", { headers: { upgrade: "websocket" } }),
        env,
      );
      expect(response.status).toBe(404);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("the upgrade header is read whatever case it arrives in", async () => {
    const { dir } = await deployment();
    try {
      const asked: string[] = [];
      const env = {
        SESSIONS: {
          getByName: (id: string) => ({
            fetch: async () => {
              asked.push(id);
              return new Response(null, { status: 101 });
            },
          }),
        },
      } as unknown as Env;
      for (const upgrade of ["websocket", "WebSocket", "WEBSOCKET"]) {
        const response = await controlPlane(
          new Request("https://x/gateway?session=s1", { headers: { upgrade } }),
          env,
        );
        expect(response.status).toBe(101);
      }
      expect(asked).toEqual(["s1", "s1", "s1"]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a stdio session never runs the host's runner, and a durable one does", async () => {
    const { dir } = await deployment();
    const runs: string[] = [];
    const runner = {
      run: async (input: { sessionId: string }) => {
        runs.push(input.sessionId);
        return { kind: "answered" as const, text: "from pi" };
      },
    };
    try {
      // A stdio route is a gateway's process, so the object records and stops.
      const stdioObject = new SessionObject(
        new DiskHost(join(dir, "stdio-runs")),
        {
          policy: routingPolicyOf({
            AGENT_DEFAULT: "claude-code",
            AGENT_RUNTIMES: "local-vm",
            AGENT_AVAILABLE_RUNTIMES: "local-vm",
          }),
        },
        runner,
      );
      await stdioObject.accept(
        SESSION,
        { provider: "linear", workspaceId: "w", channelId: "i", threadId: "sess" },
        "hello",
        "evt-1",
        10,
      );
      expect(runs).toEqual([]);
      expect((await stdioObject.turns(SESSION))[0]?.status).toBe("queued");

      // A durable route is this process. The run is detached so the webhook can
      // answer inside Linear's five-second window, so the test waits for the
      // turn to settle rather than for `accept` to return.
      const durableObject = new SessionObject(
        new DiskHost(join(dir, "durable-runs")),
        { policy: routingPolicyOf({}) },
        runner,
      );
      await durableObject.accept(
        SESSION,
        { provider: "slack", workspaceId: "T", channelId: "C", threadId: "1" },
        "hello",
        "evt-2",
        10,
      );
      await settle(() => runs.length > 0);
      expect(runs).toEqual([SESSION]);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("a message is dispatched only once it is understood", () => {
  test("a hello arriving over the socket dispatches the queued turn", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("over the socket")), env);
      const step = await onMessage(
        sessionObjects(),
        { sessionId: undefined },
        JSON.stringify({ type: "hello", sessionId: SESSION }),
        1_000,
      );
      // The handshake is what binds an otherwise anonymous connection.
      expect(step.connection).toEqual({ sessionId: SESSION });
      const envelope = JSON.parse(step.send[0] ?? "{}") as { params: { text: string } };
      expect(envelope.params.text).toBe("over the socket");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a well-formed but unknown frame leaves the connection exactly as it was", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({ jsonrpc: "2.0", method: "gateway/event", params: { type: "ping" } }),
        1_000,
      );
      expect(step).toEqual(ignoredStep(SESSION));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a message that is not a JSON-RPC notification is not read as a report", async () => {
    const { dir, sessionObjects } = await deployment();
    try {
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "1.0",
          params: { type: "turn.completed", sessionId: SESSION, turnId: "t1" },
        }),
        1_000,
      );
      // Without `method: gateway/event` the params are not unwrapped, and the
      // object has no `type` of its own, so nothing happens.
      expect(step).toEqual(ignoredStep(SESSION));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("the frame the control plane sends", () => {
  test("is a JSON-RPC 2.0 notification the gateway's codec reads", () => {
    const text = frameText({
      type: "prompt",
      sessionId: "s",
      acpSessionId: "",
      turnId: "t",
      text: "hi",
    });
    expect(JSON.parse(text)).toEqual({
      jsonrpc: "2.0",
      method: "gateway/command",
      params: { type: "prompt", sessionId: "s", acpSessionId: "", turnId: "t", text: "hi" },
    });
  });
});

describe("the settled step's exact shape", () => {
  test("a completion yields no frames, the bound connection, and the text", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_001,
      );
      expect(
        await onMessage(
          sessionObjects(),
          { sessionId: SESSION },
          JSON.stringify({
            jsonrpc: "2.0",
            method: "gateway/event",
            params: {
              type: "turn.completed",
              sessionId: SESSION,
              turnId: "t1",
              stopReason: "end_turn",
              text: "done",
            },
          }),
          1_002,
        ),
      ).toEqual({
        send: [],
        connection: { sessionId: SESSION },
        settled: { sessionId: SESSION, replyTo: "sess-local", text: "done" },
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a completion with no text settles with an empty body, not a missing one", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: { type: "turn.started", sessionId: SESSION, turnId: "t1" },
        }),
        1_001,
      );
      const step = await onMessage(
        sessionObjects(),
        { sessionId: SESSION },
        JSON.stringify({
          jsonrpc: "2.0",
          method: "gateway/event",
          params: {
            type: "turn.completed",
            sessionId: SESSION,
            turnId: "t1",
            stopReason: "end_turn",
          },
        }),
        1_002,
      );
      expect(step.settled).toEqual({ sessionId: SESSION, replyTo: "sess-local", text: "" });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("a failure yields no frames and carries the reason, not a text", async () => {
    const { dir, env, sessionObjects } = await deployment();
    try {
      await controlPlane(signedSession(sessionEvent("hello")), env);
      await onHello(sessionObjects(), SESSION, 1_000);
      expect(
        await onMessage(
          sessionObjects(),
          { sessionId: SESSION },
          JSON.stringify({
            jsonrpc: "2.0",
            method: "gateway/event",
            params: { type: "turn.failed", sessionId: SESSION, turnId: "t1", error: "boom" },
          }),
          1_001,
        ),
      ).toEqual({
        send: [],
        connection: { sessionId: SESSION },
        settled: { sessionId: SESSION, replyTo: "sess-local", error: "boom" },
      });
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

/**
 * Wait until a detached run has taken effect. `accept` returns before the run
 * because Linear needs an answer inside five seconds, so a test that asserts on
 * the run has to wait for it rather than for `accept`.
 */
async function settle(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200 && !done(); attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

describe("the deployed entry exposes the gateway socket", () => {
  test("the platform entry exports the session class and overrides fetch to accept an upgrade", async () => {
    // The object owns the socket, so the class must carry `fetch`; a missing one
    // is what the Worker's route would call and fail on.
    const source = await Bun.file(new URL("./index.ts", import.meta.url)).text();
    expect(source).toContain("export class SessionDurableObject");
    expect(source).toContain("override async fetch");
    expect(source).toContain("WebSocketPair");
    expect(source).toContain("this.#object.gateway(");
  });

  test("the Worker's gateway route hands the request to the session object", async () => {
    const source = await Bun.file(new URL("./worker.ts", import.meta.url)).text();
    // The route resolves the session's object and forwards the upgrade to it;
    // the names may wrap, so the check is on the identifiers a handoff needs.
    expect(source).toContain("env.SESSIONS.getByName(sessionId)");
    // The handoff is a `fetch` on that object, carrying the id it resolved so a
    // gateway that asked for `auto` learns which session it reached.
    expect(source).toContain("object.fetch(forwarded)");
    expect(source).toContain('new Headers([...request.headers, ["x-agent-session", sessionId]])');
    expect(source).toContain("?session=<id>");
  });

  test("the deployed entry states the local VM as its runtime", async () => {
    const config = await Bun.file(new URL("../wrangler.toml", import.meta.url)).text();
    expect(config).toContain('AGENT_RUNTIMES = "local-vm"');
    expect(config).toContain('AGENT_AVAILABLE_RUNTIMES = "local-vm"');
    expect(config).toContain('AGENT_DEFAULT = "claude-code"');
  });
});
