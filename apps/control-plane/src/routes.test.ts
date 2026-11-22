import { describe, expect, test } from "bun:test";
import { errorResponse, handleWebhook, rawRequestOf, responseFor, routeOf } from "./routes.ts";
import worker, { accept, deliveryBody, fetch, type Env } from "./worker.ts";
import { AdapterError, type AdapterResult } from "@agent/adapters";
import type { ThreadRef, Turn } from "@agent/store";
import { signSha256 } from "@agent/adapters";
import { envWith, OAUTH } from "./test-env.ts";

const config = {
  slack: { signingSecret: "shhh", botUserId: "UBOT" },
  discord: { publicKey: "aa" },
  linear: { webhookSecret: "linear" },
};

describe("routeOf", () => {
  test("maps each webhook path and health", () => {
    expect(routeOf("/webhooks/slack")).toEqual({ kind: "slack" });
    expect(routeOf("/webhooks/discord")).toEqual({ kind: "discord" });
    expect(routeOf("/webhooks/linear")).toEqual({ kind: "linear" });
    expect(routeOf("/health")).toEqual({ kind: "health" });
    expect(routeOf("/nope")).toEqual({ kind: "not-found" });
    expect(routeOf("/webhooks/slack/extra")).toEqual({ kind: "not-found" });
  });

  test("maps the two Linear OAuth paths", () => {
    expect(routeOf("/oauth/linear")).toEqual({ kind: "linear-oauth-start" });
    expect(routeOf("/oauth/linear/callback")).toEqual({ kind: "linear-oauth-callback" });
  });
});

describe("rawRequestOf", () => {
  test("lower-cases headers and reads the body once", async () => {
    const request = new Request("https://x/webhooks/slack", {
      method: "POST",
      headers: { "X-Slack-Signature": "v0=1", "Content-Type": "application/json" },
      body: '{"a":1}',
    });
    expect(await rawRequestOf(request)).toEqual({
      method: "POST",
      headers: { "x-slack-signature": "v0=1", "content-type": "application/json" },
      body: '{"a":1}',
    });
  });
});

describe("handleWebhook", () => {
  const slackRequest = async (body: unknown, at: number) => ({
    method: "POST",
    headers: {
      "x-slack-request-timestamp": String(at),
      "x-slack-signature": `v0=${await signSha256("shhh", `v0:${at}:${JSON.stringify(body)}`)}`,
    },
    body: JSON.stringify(body),
  });

  test("dispatches a verified Slack delivery", async () => {
    const body = {
      type: "event_callback",
      event_id: "Ev1",
      event: {
        type: "app_mention",
        team: "T",
        channel: "C",
        ts: "1",
        user: "U",
        text: "<@UBOT> hi",
      },
    };
    const result = await handleWebhook("slack", await slackRequest(body, 100), config, 100);
    expect(result.kind).toBe("event");
  });

  test("refuses a method other than POST", async () => {
    const error = await handleWebhook(
      "slack",
      { method: "GET", headers: {}, body: "" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(AdapterError);
    expect((error as AdapterError).status).toBe(405);
  });

  test("a route with no adapter is a 404", async () => {
    const error = await handleWebhook(
      "health",
      { method: "POST", headers: {}, body: "{}" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect((error as AdapterError).status).toBe(404);
  });
});

describe("responseFor", () => {
  test("a challenge answers verbatim", () => {
    expect(responseFor({ kind: "challenge", response: "abc" })).toEqual({
      status: 200,
      body: "abc",
    });
  });

  test("Discord's pong becomes its interaction type", () => {
    expect(responseFor({ kind: "challenge", response: "pong" })).toEqual({
      status: 200,
      body: { type: 1 },
    });
  });

  test("an accepted event is acknowledged at once", () => {
    expect(
      responseFor({
        kind: "event",
        event: {
          kind: "message",
          provider: "slack",
          eventId: "Ev1",
          thread: { workspaceId: "T", channelId: "C", threadId: "1" },
          authorId: "U",
          text: "x",
          replyTo: { channelId: "C", threadId: "1" },
        },
      }),
    ).toEqual({ status: 202, body: { ok: true, eventId: "Ev1" } });
  });

  test("an ignored delivery is acknowledged by reason", () => {
    expect(responseFor({ kind: "ignored", reason: "bot message" })).toEqual({
      status: 200,
      body: { ok: true, ignored: "bot message" },
    });
  });
});

describe("errorResponse", () => {
  test("an adapter error keeps its status", () => {
    const response = errorResponse(new AdapterError(401, "bad signature"));
    expect(response.status).toBe(401);
  });

  test("an unknown error is a 500", () => {
    expect(errorResponse(new Error("boom")).status).toBe(500);
    expect(errorResponse("string").status).toBe(500);
  });
});

describe("fetch", () => {
  const env = (outcome?: unknown) => {
    const calls: string[] = [];
    const value: Env = {
      ...config,
      ...OAUTH,
      LINEAR_OAUTH: { getByName: () => ({}) },
      THREAD_INDEX: {
        getByName: () => ({
          lookup: async (thread: ThreadRef) => {
            calls.push(`lookup:${thread.threadId}`);
            return "session-1";
          },
        }),
      },
      SESSIONS: {
        getByName: () => ({
          active: async () => ({ id: "t1", status: "running" }) as Turn,
          deliver: async () => outcome,
          transition: async () => ({ ok: true, next: { id: "t1", status: "cancelled" } }),
        }),
      },
    };
    return { env: value, calls };
  };

  const slackPost = async (body: unknown, at: number) =>
    new Request("https://x/webhooks/slack", {
      method: "POST",
      headers: {
        "x-slack-request-timestamp": String(at),
        "x-slack-signature": `v0=${await signSha256("shhh", `v0:${at}:${JSON.stringify(body)}`)}`,
      },
      body: JSON.stringify(body),
    });

  test("health answers ok", async () => {
    const response = await fetch(new Request("https://x/health"), env().env);
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ok: true });
  });

  test("an unknown path is a 404", async () => {
    const response = await fetch(new Request("https://x/nope"), env().env);
    expect(response.status).toBe(404);
    expect(await response.json()).toEqual({ error: "not found" });
  });

  test("a delivery the adapter ignores answers 200 and touches no session", async () => {
    const { env: value, calls } = env();
    const body = { type: "event_callback", event: { type: "reaction_added", team: "T" } };
    const request = await slackPost(body, 100);
    const response = await fetch(request, value, 100_000);
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ ok: true, ignored: expect.any(String) });
    expect(calls).toEqual([]);
  });

  test("a route with no adapter refuses the method through the Worker", async () => {
    const { env: value, calls } = env();
    const response = await fetch(
      new Request("https://x/webhooks/slack", { method: "GET" }),
      value,
      100_000,
    );
    expect(response.status).toBe(405);
    expect(calls).toEqual([]);
  });

  test("the health route answers any method, so a deploy check cannot fail on POST", async () => {
    const { env: value, calls } = env();
    const response = await fetch(new Request("https://x/health", { method: "POST" }), value);
    expect(response.status).toBe(200);
    expect(calls).toEqual([]);
  });

  test("a refused event still answers with its acknowledged status", async () => {
    const { env: value } = env();
    const body = {
      type: "event_callback",
      event_id: "Ev-refused",
      event: {
        type: "app_mention",
        team: "T",
        channel: "C",
        ts: "1",
        user: "U",
        text: "<@UBOT> hi",
      },
    };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(response.status).toBe(202);
    expect(await response.json()).toMatchObject({ ok: true, eventId: "Ev-refused" });
  });

  test("the Worker default export serves fetch", () => {
    expect(typeof worker.fetch).toBe("function");
  });

  test("a login request redirects through the OAuth object", async () => {
    const starts: number[] = [];
    const names: string[] = [];
    const value = envWith({
      LINEAR_OAUTH: {
        getByName: (name: string) => {
          names.push(name);
          return {
            start: async () => {
              starts.push(1);
              return "state-1";
            },
            complete: async () => {
              throw new Error("not called");
            },
          };
        },
      },
    });
    const response = await fetch(new Request("https://x/oauth/linear"), value);
    expect(response.status).toBe(302);
    expect(starts).toHaveLength(1);
    expect(names).toEqual(["linear"]);
    const location = new URL(response.headers.get("location") ?? "");
    expect(location.searchParams.get("client_id")).toBe(OAUTH.LINEAR_CLIENT_ID);
  });

  test("a callback reads the query and finishes the login through the OAuth object", async () => {
    const seen: URLSearchParams[] = [];
    const value = envWith({
      LINEAR_OAUTH: {
        getByName: () => ({
          start: async () => "",
          complete: async (params: URLSearchParams) => {
            seen.push(params);
            return {
              accessToken: "at",
              refreshToken: "rt",
              expiresIn: 86_400,
              issuedAt: 0,
              scope: "read",
              organizationId: "org-1",
            };
          },
        }),
      },
    });
    const response = await fetch(
      new Request("https://x/oauth/linear/callback?code=c1&state=s1"),
      value,
      50,
    );
    expect(response.status).toBe(200);
    expect(seen[0]?.get("code")).toBe("c1");
    expect(seen[0]?.get("state")).toBe("s1");
  });

  test("a refused callback answers its refusal and never touches a session", async () => {
    const value = envWith({
      LINEAR_OAUTH: {
        getByName: () => ({
          start: async () => "",
          complete: async () => {
            throw Object.assign(new Error("linear callback state does not match"), { status: 400 });
          },
        }),
      },
    });
    const response = await fetch(new Request("https://x/oauth/linear/callback?code=c1"), value);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({
      error: { message: "linear callback state does not match" },
    });
  });

  test("an ignored delivery answers 200, which differs from an event's 202", async () => {
    const { env: value } = env();
    const body = { type: "event_callback", event: { type: "reaction_added", team: "T" } };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(response.status).toBe(200);
  });

  test("the index is addressed by its one name", async () => {
    const names: string[] = [];
    const base = env({ kind: "accepted", session: {}, turn: {} });
    base.env.THREAD_INDEX = {
      getByName: (name: string) => {
        names.push(name);
        return {
          lookup: async () => "s1",
        };
      },
    };
    base.env.SESSIONS = { getByName: () => ({ deliver: async () => ({ kind: "accepted" }) }) };
    const body = {
      type: "event_callback",
      event_id: "Ev9",
      event: {
        type: "app_mention",
        team: "T",
        channel: "C",
        ts: "1",
        user: "U",
        text: "<@UBOT> hi",
      },
    };
    const response = await fetch(await slackPost(body, 100), base.env, 100_000);
    expect(response.status).toBe(202);
    expect(names).toEqual(["index"]);
  });

  test("a mention reaches the session and is acknowledged", async () => {
    const { env: value, calls } = env({ kind: "accepted", session: {}, turn: {} });
    const body = {
      type: "event_callback",
      event_id: "Ev1",
      event: {
        type: "app_mention",
        team: "T",
        channel: "C",
        ts: "1",
        user: "U",
        text: "<@UBOT> hi",
      },
    };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(response.status).toBe(202);
    expect(await response.json()).toEqual({
      ok: true,
      eventId: "Ev1",
      outcome: { kind: "accepted", session: {}, turn: {} },
    });
    expect(calls).toEqual(["lookup:1"]);
  });

  test("a bad signature is a 401 and no session is touched", async () => {
    const { env: value, calls } = env({});
    const request = new Request("https://x/webhooks/slack", {
      method: "POST",
      headers: { "x-slack-request-timestamp": "100", "x-slack-signature": "v0=bad" },
      body: "{}",
    });
    const response = await fetch(request, value, 100_000);
    expect(response.status).toBe(401);
    expect(calls).toEqual([]);
  });

  test("a cancel ends the active turn", async () => {
    const { env: value } = env({});
    const body = {
      type: "event_callback",
      event_id: "Ev2",
      event: {
        type: "message",
        team: "T",
        channel: "C",
        ts: "2",
        thread_ts: "1",
        user: "U",
        text: "stop",
      },
    };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(response.status).toBe(202);
  });

  test("a cancel with no active turn is refused", async () => {
    const value: Env = {
      ...config,
      ...OAUTH,
      LINEAR_OAUTH: { getByName: () => ({}) },
      THREAD_INDEX: { getByName: () => ({ lookup: async () => "session-1" }) },
      SESSIONS: {
        getByName: () => ({
          active: async () => undefined,
          deliver: async () => ({}),
          transition: async () => ({}),
        }),
      },
    };
    const body = {
      type: "event_callback",
      event_id: "Ev3",
      event: {
        type: "message",
        team: "T",
        channel: "C",
        ts: "2",
        thread_ts: "1",
        user: "U",
        text: "cancel",
      },
    };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(await response.json()).toMatchObject({
      outcome: { kind: "refused", reason: "no-active-turn" },
    });
  });

  test("a delivery the session refuses is still acknowledged", async () => {
    const { env: value } = env({ kind: "refused", reason: "closed" });
    const body = {
      type: "event_callback",
      event_id: "Ev4",
      event: {
        type: "app_mention",
        team: "T",
        channel: "C",
        ts: "1",
        user: "U",
        text: "<@UBOT> hi",
      },
    };
    const response = await fetch(await slackPost(body, 100), value, 100_000);
    expect(response.status).toBe(202);
  });
});

describe("accept", () => {
  const event = {
    kind: "message" as const,
    provider: "slack" as const,
    eventId: "Ev1",
    thread: { workspaceId: "T", channelId: "C", threadId: "1" },
    authorId: "U",
    text: "hi",
    replyTo: { channelId: "C", threadId: "1" },
  };

  test("routes a message to the session the index names", async () => {
    const seen: unknown[] = [];
    const env: Env = {
      ...config,
      ...OAUTH,
      LINEAR_OAUTH: { getByName: () => ({}) },
      THREAD_INDEX: { getByName: () => ({ lookup: async () => "session-1" }) },
      SESSIONS: {
        getByName: () => ({
          active: async () => undefined,
          deliver: async (...args: unknown[]) => {
            seen.push(args);
            return { kind: "accepted" };
          },
          transition: async () => ({}),
        }),
      },
    };
    await accept(env, event, 42);
    expect(seen).toEqual([["session-1", { provider: "slack", ...event.thread }, "hi", "Ev1", 42]]);
  });

  test("builds the thread from the provider and the event thread", async () => {
    let captured: ThreadRef | undefined;
    const env: Env = {
      ...config,
      ...OAUTH,
      LINEAR_OAUTH: { getByName: () => ({}) },
      THREAD_INDEX: { getByName: () => ({ lookup: async () => "session-1" }) },
      SESSIONS: {
        getByName: () => ({
          active: async () => undefined,
          deliver: async (_id: string, thread: ThreadRef) => {
            captured = thread;
            return { kind: "accepted" };
          },
          transition: async () => ({}),
        }),
      },
    };
    await accept(env, event, 1);
    expect(captured).toEqual({ provider: "slack", ...event.thread });
  });
});

describe("deliveryBody", () => {
  test("an ignored delivery keeps its response", () => {
    expect(deliveryBody({ kind: "ignored", reason: "x" }, { kind: "accepted" })).toEqual({
      status: 200,
      body: { ok: true, ignored: "x" },
    });
  });

  test("an event response gains the outcome", () => {
    const result: AdapterResult = {
      kind: "event",
      event: {
        kind: "cancel",
        provider: "slack",
        eventId: "Ev1",
        thread: { workspaceId: "T", channelId: "C", threadId: "1" },
        authorId: "U",
      },
    };
    expect(deliveryBody(result, { ok: true })).toEqual({
      status: 202,
      body: { ok: true, eventId: "Ev1", outcome: { ok: true } },
    });
  });
});

describe("diagnostics", () => {
  test("a non-POST method names the method", async () => {
    const error = await handleWebhook(
      "slack",
      { method: "PUT", headers: {}, body: "" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect((error as AdapterError).message).toBe("only POST is accepted");
  });

  test("a route with no adapter names the route", async () => {
    const error = await handleWebhook(
      "health",
      { method: "POST", headers: {}, body: "{}" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect((error as AdapterError).message).toBe("no adapter for this route");
  });

  test("a Discord delivery reaches the Discord adapter", async () => {
    const result = await handleWebhook(
      "discord",
      { method: "POST", headers: {}, body: "{}" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect((result as AdapterError).status).toBe(401);
  });

  test("a Linear delivery reaches the Linear adapter", async () => {
    const result = await handleWebhook(
      "linear",
      { method: "POST", headers: {}, body: "{}" },
      config,
      0,
    ).catch((reason: unknown) => reason);
    expect((result as AdapterError).message).toBe("missing Linear signature");
  });

  test("an adapter error body carries the message", async () => {
    const response = errorResponse(new AdapterError(403, "nope"));
    expect(response.status).toBe(403);
    expect(await response.json()).toEqual({ error: { message: "nope" } });
  });

  test("an unknown error body carries its message", async () => {
    const response = errorResponse(new TypeError("bad"));
    expect(await response.json()).toEqual({ error: { message: "bad" } });
    expect(await errorResponse("plain").json()).toEqual({ error: { message: "plain" } });
  });
});
