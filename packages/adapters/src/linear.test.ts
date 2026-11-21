import { describe, expect, test } from "bun:test";
import { adaptLinear, parseLinearEvent, verifyLinear } from "./linear.ts";
import { AdapterError, type RawRequest } from "./events.ts";
import { hmacHex } from "./signature.ts";

const config = { webhookSecret: "linear-secret" };

async function signed(body: unknown): Promise<RawRequest> {
  const text = JSON.stringify(body);
  return {
    method: "POST",
    headers: { "linear-signature": await hmacHex(config.webhookSecret, text, "sha256") },
    body: text,
  };
}

async function rejection(promise: Promise<unknown>): Promise<AdapterError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(AdapterError);
    return error as AdapterError;
  }
  throw new Error("expected an AdapterError");
}

const sessionBody = (overrides: Record<string, unknown> = {}) => ({
  type: "AgentSessionEvent",
  action: "created",
  webhookId: "wh-1",
  agentSession: { id: "sess-9" },
  issue: { id: "issue-3", teamId: "team-1" },
  actor: { id: "user-7" },
  agentActivity: { type: "prompt", body: "add retries to the uploader" },
  ...overrides,
});

describe("verifyLinear", () => {
  test("accepts a correctly signed body", async () => {
    await verifyLinear(await signed(sessionBody()), config);
  });

  test("rejects a missing signature header", async () => {
    const request = await signed(sessionBody());
    expect((await rejection(verifyLinear({ ...request, headers: {} }, config))).status).toBe(401);
  });

  test("rejects a tampered body", async () => {
    const request = await signed(sessionBody());
    const tampered = { ...request, body: request.body.replace("add retries", "remove retries") };
    expect((await rejection(verifyLinear(tampered, config))).status).toBe(401);
  });

  test("rejects a signature from another secret", async () => {
    expect(
      (await rejection(verifyLinear(await signed(sessionBody()), { webhookSecret: "other" })))
        .status,
    ).toBe(401);
  });
});

describe("parseLinearEvent", () => {
  test("turns an agent session event into a message", () => {
    expect(parseLinearEvent(sessionBody())).toEqual({
      kind: "event",
      event: {
        kind: "message",
        provider: "linear",
        eventId: "wh-1",
        thread: { workspaceId: "team-1", channelId: "issue-3", threadId: "sess-9" },
        authorId: "user-7",
        text: "add retries to the uploader",
        replyTo: { channelId: "issue-3", threadId: "sess-9" },
      },
    });
  });

  test("reads a top-level prompt when there is no agent activity", () => {
    const result = parseLinearEvent(sessionBody({ agentActivity: undefined, prompt: "top level" }));
    expect(result.kind === "event" && result.event.kind === "message" && result.event.text).toBe(
      "top level",
    );
  });

  test("ignores a thought echo from the agent", () => {
    expect(
      parseLinearEvent(sessionBody({ agentActivity: { type: "thought", body: "thinking" } })),
    ).toEqual({
      kind: "ignored",
      reason: "Linear activity echo",
    });
  });

  test("ignores an event with no prompt", () => {
    const result = parseLinearEvent(
      sessionBody({ agentActivity: { type: "prompt", body: "   " } }),
    );
    expect(result).toEqual({ kind: "ignored", reason: "Linear event has no prompt" });
  });

  test("ignores an unrelated Linear event type", () => {
    expect(parseLinearEvent({ type: "Issue" })).toEqual({
      kind: "ignored",
      reason: "unsupported Linear event: Issue",
    });
  });

  test("falls back to the session id when the payload has no webhook id", () => {
    const result = parseLinearEvent(sessionBody({ webhookId: undefined }));
    expect(result.kind === "event" && result.event.eventId).toBe("sess-9");
  });

  test("reads the author from the user field when there is no actor", () => {
    const result = parseLinearEvent(sessionBody({ actor: undefined, user: { id: "user-2" } }));
    expect(result.kind === "event" && result.event.authorId).toBe("user-2");
  });

  test("an event with no author has an empty author id", () => {
    const result = parseLinearEvent(sessionBody({ actor: undefined }));
    expect(result.kind === "event" && result.event.authorId).toBe("");
  });

  test("rejects a payload that is not an object or has no session or issue", () => {
    expect(() => parseLinearEvent(null)).toThrow(AdapterError);
    expect(() => parseLinearEvent([])).toThrow(AdapterError);
    expect(() => parseLinearEvent({ type: "AgentSessionEvent" })).toThrow(AdapterError);
    expect(() => parseLinearEvent(sessionBody({ agentSession: {} }))).toThrow(AdapterError);
    expect(() => parseLinearEvent(sessionBody({ issue: {} }))).toThrow(AdapterError);
    expect(() => parseLinearEvent(sessionBody({ issue: { id: "i" } }))).toThrow(AdapterError);
  });
});

describe("adaptLinear", () => {
  test("verifies then parses", async () => {
    expect((await adaptLinear(await signed(sessionBody()), config)).kind).toBe("event");
  });

  test("a signature failure is a 401 before parsing", async () => {
    const request = { ...(await signed(sessionBody())), body: "nope" };
    expect((await rejection(adaptLinear(request, config))).status).toBe(401);
  });

  test("a signed body that is not JSON is a 400", async () => {
    const text = "nope";
    const request = {
      method: "POST",
      headers: { "linear-signature": await hmacHex(config.webhookSecret, text, "sha256") },
      body: text,
    };
    expect((await rejection(adaptLinear(request, config))).status).toBe(400);
  });
});
