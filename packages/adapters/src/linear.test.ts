import { describe, expect, test } from "bun:test";
import { adaptLinear, parseLinearEvent, verifyLinear } from "./linear.ts";
import type { AdapterResult, RawRequest } from "./events.ts";
import type { Signature, Verified } from "./signature.ts";
import { hmacHex } from "./signature.ts";

const config = { webhookSecret: "linear-secret" };

async function signed(body: unknown): Promise<RawRequest> {
  const text = JSON.stringify(body);
  return {
    method: "POST",
    headers: { "linear-signature": signedHex(await hmacHex(config.webhookSecret, text, "sha256")) },
    body: text,
  };
}

/** The hex a signature produced, or a failure naming the refusal. */
function signedHex(result: Signature): string {
  if (result.kind !== "signed") throw new Error("expected a signature");
  return result.hex;
}

/** The status a verification refusal answers with, or 200 when it accepted. */
function verificationStatus(answer: Verified): number {
  return answer.kind === "verified" ? 200 : 401;
}

/** The refusal an adapter returned, or a failure naming what it returned instead. */
function refused(result: AdapterResult): { status: number; message: string } {
  if (result.kind !== "refused") throw new Error("expected a refusal");
  return { status: result.status, message: result.message };
}

const sessionBody = (overrides: Record<string, unknown> = {}) => ({
  type: "AgentSessionEvent",
  action: "created",
  webhookId: "wh-1",
  organizationId: "org-1",
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
    expect(verificationStatus(await verifyLinear({ ...request, headers: {} }, config))).toBe(401);
  });

  test("rejects a tampered body", async () => {
    const request = await signed(sessionBody());
    const tampered = { ...request, body: request.body.replace("add retries", "remove retries") };
    expect(verificationStatus(await verifyLinear(tampered, config))).toBe(401);
  });

  test("rejects a signature from another secret", async () => {
    expect(
      verificationStatus(
        await verifyLinear(await signed(sessionBody()), { webhookSecret: "other" }),
      ),
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
        thread: { workspaceId: "org-1", channelId: "issue-3", threadId: "sess-9" },
        authorId: "user-7",
        text: "add retries to the uploader",
        replyTo: { channelId: "issue-3", threadId: "sess-9" },
        stage: "start",
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

  test("rejects a payload that is not an object or has no session", () => {
    for (const body of [
      null,
      [],
      { type: "AgentSessionEvent" },
      sessionBody({ agentSession: {} }),
    ]) {
      expect(refused(parseLinearEvent(body)).status).toBe(400);
    }
  });

  test("a prompted event has no issue, so the session is the thread", () => {
    const result = parseLinearEvent(sessionBody({ action: "prompted", issue: undefined }));
    expect(result.kind === "event" && result.event).toMatchObject({
      stage: "prompt",
      thread: { channelId: "sess-9", threadId: "sess-9" },
      replyTo: { channelId: "sess-9", threadId: "sess-9" },
    });
  });

  test("a created event starts a session", () => {
    const result = parseLinearEvent(sessionBody({ action: "created" }));
    expect(result.kind === "event" && "stage" in result.event && result.event.stage).toBe("start");
  });

  test("an unknown action starts a session, because a session exists either way", () => {
    const result = parseLinearEvent(sessionBody({ action: undefined }));
    expect(result.kind === "event" && "stage" in result.event && result.event.stage).toBe("start");
  });

  test("an issue with an empty id falls back to the session, so the thread is never empty", () => {
    const result = parseLinearEvent(sessionBody({ issue: { id: "" } }));
    expect(result.kind === "event" && result.event.thread.channelId).toBe("sess-9");
  });

  test("an event with no organization has an empty workspace rather than a missing field", () => {
    const result = parseLinearEvent(sessionBody({ organizationId: undefined }));
    expect(result.kind === "event" && result.event.thread.workspaceId).toBe("");
  });

  test("an issue with an empty id falls back to the session, so the thread is never empty", () => {
    const result = parseLinearEvent(sessionBody({ issue: undefined }));
    expect(result.kind === "event" && result.event.thread.channelId).toBe("sess-9");
  });
});

describe("adaptLinear", () => {
  test("verifies then parses", async () => {
    expect((await adaptLinear(await signed(sessionBody()), config)).kind).toBe("event");
  });

  test("a signature failure is a 401 before parsing", async () => {
    const request = { ...(await signed(sessionBody())), body: "nope" };
    expect(refused(await adaptLinear(request, config)).status).toBe(401);
  });

  test("a signed body that is not JSON is a 400", async () => {
    const text = "nope";
    const request = {
      method: "POST",
      headers: {
        "linear-signature": signedHex(await hmacHex(config.webhookSecret, text, "sha256")),
      },
      body: text,
    };
    expect(refused(await adaptLinear(request, config)).status).toBe(400);
  });
});

/**
 * Linear's published `AgentSessionEventWebhookPayload`. These fixtures come from
 * the schema and from a payload the deployed Worker received, so a decoder that
 * only reads a hand-written shape fails here.
 */
const realSessionEvent = (overrides: Record<string, unknown> = {}) => ({
  type: "AgentSessionEvent",
  action: "created",
  webhookId: "wh-real",
  organizationId: "org-6d72de72",
  appUserId: "app-1de62928",
  promptContext:
    "Issue MOK-279: test issue\n\nthis is test. if you have seen this issue, reply `echo for you`.",
  agentSession: {
    id: "sess-real",
    issue: {
      id: "7cc41723-f197-47b7-b67e-0a7d8a5a216c",
      identifier: "MOK-279",
      title: "test issue",
    },
    creatorId: "user-91c61c96",
  },
  actor: { id: "user-91c61c96", type: "user" },
  ...overrides,
});

describe("the payload Linear actually sends", () => {
  test("a created session reads the issue from inside the session, not the top level", () => {
    const result = parseLinearEvent(realSessionEvent());
    if (result.kind !== "event") throw new Error("expected an event");
    expect(result.event.thread.channelId).toBe("7cc41723-f197-47b7-b67e-0a7d8a5a216c");
    expect(result.event.thread.threadId).toBe("sess-real");
    if (result.event.kind !== "message") throw new Error("expected a message");
    expect(result.event.replyTo.channelId).toBe("7cc41723-f197-47b7-b67e-0a7d8a5a216c");
  });

  test("a created session uses promptContext as the task, because it carries the issue and its guidance", () => {
    const result = parseLinearEvent(realSessionEvent());
    if (result.kind !== "event" || result.event.kind !== "message")
      throw new Error("expected a message");
    expect(result.event.text).toBe(realSessionEvent().promptContext);
    expect(result.event.stage).toBe("start");
  });

  test("a prompted session reads the person's message from agentActivity.body", () => {
    const result = parseLinearEvent(
      realSessionEvent({
        action: "prompted",
        promptContext: undefined,
        agentActivity: { type: "prompt", body: "now add a comment" },
      }),
    );
    if (result.kind !== "event" || result.event.kind !== "message") {
      throw new Error("expected a message");
    }
    expect(result.event.text).toBe("now add a comment");
    expect(result.event.stage).toBe("prompt");
  });

  test("the actor is the author, and the session creator is the fallback", () => {
    const withActor = parseLinearEvent(realSessionEvent());
    if (withActor.kind !== "event") throw new Error("expected an event");
    expect(withActor.event.authorId).toBe("user-91c61c96");

    const withoutActor = parseLinearEvent(realSessionEvent({ actor: undefined }));
    if (withoutActor.kind !== "event") throw new Error("expected an event");
    expect(withoutActor.event.authorId).toBe("user-91c61c96");
  });

  test("an Issue event carries no session, so it is ignored by name", () => {
    // The state change that triggered this work arrived as an Issue event.
    expect(
      parseLinearEvent({
        type: "Issue",
        action: "update",
        organizationId: "org-1",
        webhookId: "wh-issue",
        actor: { id: "user-1", type: "user" },
        data: {
          id: "7cc41723-f197-47b7-b67e-0a7d8a5a216c",
          identifier: "MOK-279",
          title: "test issue",
        },
      }),
    ).toEqual({ kind: "ignored", reason: "unsupported Linear event: Issue" });
  });

  test("a session with no prompt anywhere is ignored rather than opening an empty turn", () => {
    expect(parseLinearEvent(realSessionEvent({ promptContext: undefined }))).toEqual({
      kind: "ignored",
      reason: "Linear event has no prompt",
    });
  });

  test("a thought we emitted ourselves is not answered", () => {
    expect(
      parseLinearEvent(
        realSessionEvent({ agentActivity: { type: "thought", body: "Working on it." } }),
      ),
    ).toEqual({ kind: "ignored", reason: "Linear activity echo" });
  });

  test("a session the payload does not name is refused with a 400", () => {
    expect(
      refused(parseLinearEvent({ type: "AgentSessionEvent", action: "created" })).message,
    ).toBe("Linear event has no agent session");
  });

  test("a session with no issue answers on the session id, so nothing is dropped", () => {
    const result = parseLinearEvent(
      realSessionEvent({ agentSession: { id: "sess-only", creatorId: "user-1" } }),
    );
    if (result.kind !== "event") throw new Error("expected an event");
    expect(result.event.thread.channelId).toBe("sess-only");
  });

  test("a title alone is not a prompt, because the task is in promptContext", () => {
    // An issue in the payload without promptContext has no task text; the turn
    // must not be created from a title the agent cannot act on.
    const result = parseLinearEvent(
      realSessionEvent({ promptContext: undefined, agentActivity: undefined }),
    );
    expect(result).toEqual({ kind: "ignored", reason: "Linear event has no prompt" });
  });
});
