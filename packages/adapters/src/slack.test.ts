import { describe, expect, test } from "bun:test";
import { adaptSlack, parseSlackEvent, SLACK_MAX_SKEW_SECONDS, verifySlack } from "./slack.ts";
import { stripMention } from "./slack-schema.ts";
import type { AdapterResult, RawRequest } from "./events.ts";
import type { Signature, Verified } from "./signature.ts";
import { signSha256 } from "./signature.ts";

const config = { signingSecret: "shhh", botUserId: "UBOT" };

async function signedRequest(body: unknown, at = 1_700_000_000): Promise<RawRequest> {
  const text = JSON.stringify(body);
  const signature = `v0=${signedHex(await signSha256(config.signingSecret, `v0:${at}:${text}`))}`;
  return {
    method: "POST",
    headers: { "x-slack-request-timestamp": String(at), "x-slack-signature": signature },
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

const mentionBody = (overrides: Record<string, unknown> = {}) => ({
  type: "event_callback",
  event_id: "Ev1",
  event: {
    type: "app_mention",
    team: "T1",
    channel: "C1",
    ts: "100.1",
    user: "U1",
    text: "<@UBOT> please fix the build",
    ...overrides,
  },
});

describe("verifySlack", () => {
  test("accepts a correctly signed, recent request", async () => {
    const now = 1_700_000_000;
    await verifySlack(await signedRequest(mentionBody(), now), config, now);
  });

  test("accepts a request at the edge of the skew window", async () => {
    const now = 1_700_000_000;
    await verifySlack(
      await signedRequest(mentionBody(), now - SLACK_MAX_SKEW_SECONDS),
      config,
      now,
    );
    await verifySlack(
      await signedRequest(mentionBody(), now + SLACK_MAX_SKEW_SECONDS),
      config,
      now,
    );
  });

  test("rejects a request one second past the window", async () => {
    const now = 1_700_000_000;
    const stale = await signedRequest(mentionBody(), now - SLACK_MAX_SKEW_SECONDS - 1);
    expect(verificationStatus(await verifySlack(stale, config, now))).toBe(401);
  });

  test("rejects a missing timestamp or signature", async () => {
    const request = await signedRequest(mentionBody());
    expect(
      verificationStatus(await verifySlack({ ...request, headers: {} }, config, 1_700_000_000)),
    ).toBe(401);
    expect(
      verificationStatus(
        await verifySlack(
          { ...request, headers: { "x-slack-request-timestamp": "1700000000" } },
          config,
          1_700_000_000,
        ),
      ),
    ).toBe(401);
  });

  test("rejects a non-numeric timestamp", async () => {
    const request = await signedRequest(mentionBody());
    const broken = {
      ...request,
      headers: { ...request.headers, "x-slack-request-timestamp": "soon" },
    };
    expect(verificationStatus(await verifySlack(broken, config, 1_700_000_000))).toBe(401);
  });

  test("rejects a tampered body", async () => {
    const request = await signedRequest(mentionBody());
    const tampered = {
      ...request,
      body: request.body.replace("fix the build", "delete everything"),
    };
    expect(verificationStatus(await verifySlack(tampered, config, 1_700_000_000))).toBe(401);
  });

  test("rejects a signature made with another secret", async () => {
    const request = await signedRequest(mentionBody());
    expect(
      verificationStatus(
        await verifySlack(request, { ...config, signingSecret: "other" }, 1_700_000_000),
      ),
    ).toBe(401);
  });
});

describe("parseSlackEvent", () => {
  test("answers url_verification with its challenge", () => {
    expect(parseSlackEvent({ type: "url_verification", challenge: "abc123" }, config)).toEqual({
      kind: "challenge",
      response: "abc123",
    });
  });

  test("turns an app_mention into a message without the mention", () => {
    expect(parseSlackEvent(mentionBody(), config)).toEqual({
      kind: "event",
      event: {
        kind: "message",
        provider: "slack",
        eventId: "Ev1",
        thread: { workspaceId: "T1", channelId: "C1", threadId: "100.1" },
        authorId: "U1",
        text: "please fix the build",
        replyTo: { channelId: "C1", threadId: "100.1" },
        stage: "message",
      },
    });
  });

  test("a mention inside a thread replies to the thread parent", () => {
    const body = mentionBody({ thread_ts: "99.0" });
    const result = parseSlackEvent(body, config);
    expect(
      result.kind === "event" && result.event.kind === "message" && result.event.thread.threadId,
    ).toBe("99.0");
  });

  test("ignores a message from the bot itself", () => {
    expect(parseSlackEvent(mentionBody({ user: "UBOT" }), config)).toEqual({
      kind: "ignored",
      reason: "bot message",
    });
  });

  test("a thread reply becomes a message", () => {
    const body = {
      type: "event_callback",
      event_id: "Ev2",
      event: {
        type: "message",
        team: "T1",
        channel: "C1",
        ts: "101.0",
        thread_ts: "100.1",
        user: "U1",
        text: "also update the docs",
      },
    };
    const result = parseSlackEvent(body, config);
    expect(result.kind === "event" && result.event.kind).toBe("message");
  });

  test("a thread reply saying stop becomes a cancel", () => {
    const body = {
      type: "event_callback",
      event_id: "Ev3",
      event: {
        type: "message",
        team: "T1",
        channel: "C1",
        ts: "102.0",
        thread_ts: "100.1",
        user: "U1",
        text: "Cancel",
      },
    };
    expect(parseSlackEvent(body, config)).toEqual({
      kind: "event",
      event: {
        kind: "cancel",
        provider: "slack",
        eventId: "Ev3",
        thread: { workspaceId: "T1", channelId: "C1", threadId: "100.1" },
        authorId: "U1",
      },
    });
  });

  test("ignores a top-level message that is not a thread reply", () => {
    const body = {
      type: "event_callback",
      event: { type: "message", team: "T1", channel: "C1", ts: "1.0", user: "U1", text: "hi" },
    };
    expect(parseSlackEvent(body, config)).toEqual({
      kind: "ignored",
      reason: "not a thread reply",
    });
  });

  test("ignores the thread parent message echoed back", () => {
    const body = {
      type: "event_callback",
      event: {
        type: "message",
        team: "T1",
        channel: "C1",
        ts: "100.1",
        thread_ts: "100.1",
        user: "U1",
        text: "parent",
      },
    };
    expect(parseSlackEvent(body, config)).toEqual({ kind: "ignored", reason: "thread parent" });
  });

  test("ignores an unsupported event type", () => {
    expect(
      parseSlackEvent({ type: "event_callback", event: { type: "reaction_added" } }, config),
    ).toEqual({
      kind: "ignored",
      reason: "unsupported Slack event: reaction_added",
    });
  });

  test("falls back to a synthesized event id when Slack sends none", () => {
    const body = mentionBody();
    delete (body as { event_id?: string }).event_id;
    const result = parseSlackEvent(body, config);
    expect(result.kind === "event" && result.event.eventId).toBe("C1:100.1");
  });

  test("rejects malformed payloads", () => {
    for (const body of [
      null,
      [],
      "text",
      {},
      { type: "event_callback" },
      { type: "event_callback", event: {} },
    ]) {
      expect(refused(parseSlackEvent(body, config)).status).toBe(400);
    }
  });

  test("rejects an event with no team, channel, or timestamp", () => {
    for (const override of [{ team: undefined }, { channel: undefined }, { ts: undefined }]) {
      expect(refused(parseSlackEvent(mentionBody(override), config)).status).toBe(400);
    }
  });

  test("a non-string text becomes an empty prompt rather than a crash", () => {
    const result = parseSlackEvent(mentionBody({ text: 42 }), config);
    expect(result.kind === "event" && result.event.kind === "message" && result.event.text).toBe(
      "",
    );
  });
});

describe("stripMention", () => {
  test("removes every mention and collapses whitespace", () => {
    expect(stripMention("<@UBOT>  fix   this <@UBOT> now ", "UBOT")).toBe("fix this now");
  });

  test("leaves text without a mention untouched apart from whitespace and trim", () => {
    expect(stripMention("  plain text  ", "UBOT")).toBe("plain text");
  });

  test("an empty result for a mention-only message", () => {
    expect(stripMention("<@UBOT>", "UBOT")).toBe("");
  });
});

describe("adaptSlack", () => {
  test("verifies then parses", async () => {
    const now = 1_700_000_000;
    const result = await adaptSlack(await signedRequest(mentionBody(), now), config, now);
    expect(result.kind).toBe("event");
  });

  test("a bad signature fails before the body is parsed", async () => {
    const now = 1_700_000_000;
    const request = { ...(await signedRequest(mentionBody(), now)), body: "not json" };
    expect(refused(await adaptSlack(request, config, now)).status).toBe(401);
  });

  test("a signed body that is not JSON is a 400", async () => {
    const now = 1_700_000_000;
    const text = "not json";
    const signature = `v0=${signedHex(await signSha256(config.signingSecret, `v0:${now}:${text}`))}`;
    const request = {
      method: "POST",
      headers: { "x-slack-request-timestamp": String(now), "x-slack-signature": signature },
      body: text,
    };
    expect(refused(await adaptSlack(request, config, now)).status).toBe(400);
  });
});

describe("more slack edges", () => {
  test("a thread reply with no timestamp is a 400", () => {
    const body = { event: { type: "message", team: "T", channel: "C", thread_ts: "1", user: "U" } };
    expect(failureText(parseSlackEvent(body, config))).toBe("Slack message has no timestamp");
  });

  test("a cancel without an event id falls back to the thread", () => {
    const body = {
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
    const result = parseSlackEvent(body, config);
    expect(result.kind === "event" && result.event.kind === "cancel" && result.event.eventId).toBe(
      "C:1",
    );
  });

  test("a cancel from the bot is ignored as a bot message", () => {
    const body = {
      event: {
        type: "message",
        team: "T",
        channel: "C",
        ts: "2",
        thread_ts: "1",
        user: "UBOT",
        text: "stop",
      },
    };
    expect(parseSlackEvent(body, config)).toEqual({ kind: "ignored", reason: "bot message" });
  });

  test("a url_verification body is answered even with other fields", () => {
    const body = { type: "url_verification", challenge: "c", event: { type: "x" } };
    expect(parseSlackEvent(body, config)).toEqual({ kind: "challenge", response: "c" });
  });
});

/** The message a refusal carries, so a test asserts on the diagnostic. */
function failureText(result: AdapterResult): string {
  return refused(result).message;
}
