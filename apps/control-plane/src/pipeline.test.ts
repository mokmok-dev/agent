import { describe, expect, test } from "bun:test";
import { createHmac } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { onHello, onMessage } from "./gateway-connection.ts";
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
      expect(session?.selection).toMatchObject({
        runtime: "local-vm",
        protocol: "stdio",
        agent: "claude-code",
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
      expect(step.send).toHaveLength(1);
      // The frame is a JSON-RPC notification, because the gateway parses every
      // message with the ACP codec; `params` is the command itself.
      const envelope = JSON.parse(step.send[0] ?? "{}") as {
        method: string;
        params: Record<string, unknown>;
      };
      expect(envelope.method).toBe("gateway/command");
      expect(envelope.params).toMatchObject({
        type: "prompt",
        sessionId: SESSION,
        turnId: "t1",
        text: "write the note",
        acpSessionId: "",
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
    expect(frame).toEqual({
      type: "prompt",
      sessionId: SESSION,
      acpSessionId: "acp-1",
      turnId: "t2",
      text: "text",
    });
  });
});
