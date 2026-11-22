import { describe, expect, test } from "bun:test";
import type { ActivityRequest } from "@agent/adapters";
import type { LinearCredential } from "./linear-oauth-do.ts";
import type { LinearOAuthPort } from "./linear-oauth-port.ts";
import { linearReplies, postActivity } from "./linear-reply.ts";

/** A credential store that answers with a fixed value, or with none. */
function oauth(credential: LinearCredential | undefined): LinearOAuthPort {
  return {
    start: async () => undefined,
    complete: async () => ({ kind: "rejected", status: 400, message: "not used" }),
    status: async () => credential,
    accessToken: async () =>
      credential === undefined ? undefined : { kind: "token", token: credential.accessToken },
  };
}

const credential: LinearCredential = {
  accessToken: "at-1",
  refreshToken: "rt-1",
  expiresIn: 86_400,
  issuedAt: 0,
  scope: "read write",
  organizationId: "app-1",
};

/** Records each GraphQL call and answers as told. */
function endpoint(answer: unknown): {
  request: ActivityRequest;
  calls: { body: string; auth: string }[];
} {
  const calls: { body: string; auth: string }[] = [];
  return {
    calls,
    request: async (_url, init) => {
      calls.push({ body: init.body, auth: init.headers["authorization"] ?? "" });
      return answer;
    },
  };
}

const accepted = { data: { agentActivityCreate: { success: true, agentActivity: { id: "a-1" } } } };

describe("acknowledging", () => {
  test("sends a thought for the session with the stored token", async () => {
    const { request, calls } = endpoint(accepted);
    const outcome = await linearReplies(oauth(credential), request).acknowledge("sess-9", 10);
    expect(outcome).toEqual({ kind: "sent", activityId: "a-1" });
    const sent = JSON.parse(calls[0]?.body ?? "{}");
    expect(sent.variables.input.agentSessionId).toBe("sess-9");
    expect(sent.variables.input.content).toEqual({ type: "thought", body: "Working on it." });
    expect(calls[0]?.auth).toBe("Bearer at-1");
  });

  test("does nothing without a credential, so a delivery still succeeds", async () => {
    const { request, calls } = endpoint(accepted);
    expect(await linearReplies(oauth(undefined), request).acknowledge("s", 0)).toEqual({
      kind: "skipped",
      reason: "no-credential",
    });
    expect(calls).toEqual([]);
  });

  test("reports Linear's refusal instead of throwing", async () => {
    const { request } = endpoint({ errors: [{ message: "bad scope" }] });
    expect(await linearReplies(oauth(credential), request).acknowledge("s", 0)).toEqual({
      kind: "failed",
      error: "bad scope",
    });
  });
});

describe("answering", () => {
  test("sends the agent's words as a response", async () => {
    const { request, calls } = endpoint(accepted);
    await linearReplies(oauth(credential), request).answer("sess-9", "The build is green.", 10);
    const sent = JSON.parse(calls[0]?.body ?? "{}");
    expect(sent.variables.input.content).toEqual({
      type: "response",
      body: "The build is green.",
    });
  });

  test("an unreachable Linear is reported, so a turn is not lost", async () => {
    const replies = linearReplies(oauth(credential), async () => {
      throw new Error("offline");
    });
    expect(await replies.answer("s", "hi", 0)).toEqual({
      kind: "failed",
      error: expect.stringContaining("linear unreachable"),
    });
  });

  test("an answer with no credential is skipped", async () => {
    const { request } = endpoint(accepted);
    expect(await linearReplies(oauth(undefined), request).answer("s", "hi", 0)).toMatchObject({
      kind: "skipped",
    });
  });

  test("a response Linear accepts without an id still counts as sent", async () => {
    const { request } = endpoint({ data: { agentActivityCreate: { success: true } } });
    expect(await linearReplies(oauth(credential), request).answer("s", "hi", 0)).toEqual({
      kind: "sent",
      activityId: null,
    });
  });
});

describe("the default transport", () => {
  test("posts to Linear and reads the JSON answer", async () => {
    const seen: { method: string; body: string; auth: string }[] = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        seen.push({
          method: request.method,
          body: await request.text(),
          auth: request.headers.get("authorization") ?? "",
        });
        return Response.json({ data: { agentActivityCreate: { success: true } } });
      },
    });
    try {
      const body = await postActivity(`http://127.0.0.1:${server.port}/graphql`, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: "Bearer at-1" },
        body: JSON.stringify({ query: "q" }),
      });
      expect(body).toEqual({ data: { agentActivityCreate: { success: true } } });
      expect(seen).toEqual([
        { method: "POST", body: JSON.stringify({ query: "q" }), auth: "Bearer at-1" },
      ]);
    } finally {
      server.stop(true);
    }
  });
});

describe("a token renewal that refused", () => {
  test("the reply fails with the renewal's reason, so the turn can report it", async () => {
    const replies = linearReplies({
      start: async () => undefined,
      complete: async () => ({ kind: "rejected", status: 400, message: "not used" }),
      status: async () => credential,
      accessToken: async () => ({ kind: "unreachable", message: "offline" }),
    });
    expect(await replies.answer("s", "hi", 0)).toEqual({ kind: "failed", error: "offline" });
  });
});
