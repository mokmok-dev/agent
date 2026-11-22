import type {
  AdapterResult,
  InboundCancel,
  InboundMessage,
  RawRequest,
  ThreadId,
} from "./events.ts";
import { AdapterError, decodeOr400 } from "./events.ts";
import { isCancelText } from "./commands.ts";
import { constantTimeEqual, signSha256 } from "./signature.ts";
import {
  decodeSlackEnvelope,
  promptOf,
  threadOf,
  type SlackEnvelope,
  type SlackMessage,
} from "./slack-schema.ts";

/** Slack's signature header is `v0=<hex hmac>`, valid for five minutes. */
export const SLACK_MAX_SKEW_SECONDS = 300;

export type SlackConfig = { signingSecret: string; botUserId: string };

export function parseJson(request: RawRequest): unknown {
  try {
    return JSON.parse(request.body);
  } catch {
    throw new AdapterError(400, "body is not JSON");
  }
}

/**
 * Verify a Slack request: a `v0` HMAC over `v0:<timestamp>:<body>`, and a
 * timestamp inside the skew window so a captured request cannot be replayed
 * later.
 */
export async function verifySlack(
  request: RawRequest,
  config: SlackConfig,
  nowSeconds: number,
): Promise<void> {
  const timestamp = request.headers["x-slack-request-timestamp"];
  const signature = request.headers["x-slack-signature"];
  if (timestamp === undefined) throw new AdapterError(401, "missing Slack timestamp header");
  if (signature === undefined) throw new AdapterError(401, "missing Slack signature header");
  const seconds = Number(timestamp);
  if (!Number.isFinite(seconds)) throw new AdapterError(401, "invalid Slack timestamp");
  if (Math.abs(nowSeconds - seconds) > SLACK_MAX_SKEW_SECONDS) {
    throw new AdapterError(401, "Slack request is outside the allowed age");
  }
  const expected = `v0=${await signSha256(config.signingSecret, `v0:${timestamp}:${request.body}`)}`;
  if (!constantTimeEqual(expected, signature)) throw new AdapterError(401, "invalid signature");
}

export function parseSlackEvent(body: unknown, config: SlackConfig): AdapterResult {
  const envelope = decodeOr400(decodeSlackEnvelope, body);
  if (envelope.type === "url_verification") {
    if (envelope.challenge === undefined) throw new AdapterError(400, "missing Slack challenge");
    return { kind: "challenge", response: envelope.challenge };
  }
  if (envelope.payload !== undefined)
    return { kind: "ignored", reason: "Slack interactive payload" };
  const event = envelope.event;
  if (event === undefined) throw new AdapterError(400, "missing Slack event");
  if (event.type === "app_mention") return mentionEvent(envelope, event, config);
  return messageEvent(envelope, event, config);
}

function mentionEvent(
  envelope: SlackEnvelope,
  event: SlackMessage,
  config: SlackConfig,
): AdapterResult {
  const thread = threadOf(event);
  if (thread === undefined) {
    throw new AdapterError(400, "Slack mention is missing team, channel, or timestamp");
  }
  if (event.user === config.botUserId) return { kind: "ignored", reason: "bot message" };
  return messageResult(envelope, thread, event, config);
}

function messageEvent(
  envelope: SlackEnvelope,
  event: SlackMessage,
  config: SlackConfig,
): AdapterResult {
  if (event.type !== "message") {
    return { kind: "ignored", reason: `unsupported Slack event: ${event.type}` };
  }
  if (event.thread_ts === undefined) return { kind: "ignored", reason: "not a thread reply" };
  if (event.ts === undefined) throw new AdapterError(400, "Slack message has no timestamp");
  if (event.ts === event.thread_ts) return { kind: "ignored", reason: "thread parent" };
  if (event.user === config.botUserId) return { kind: "ignored", reason: "bot message" };
  const thread = threadOf(event);
  if (thread === undefined) {
    throw new AdapterError(400, "Slack message is missing team, channel, or timestamp");
  }
  return messageResult(envelope, thread, event, config);
}

function messageResult(
  envelope: SlackEnvelope,
  thread: ThreadId,
  event: SlackMessage,
  config: SlackConfig,
): AdapterResult {
  const eventId = envelope.event_id ?? `${thread.channelId}:${thread.threadId}`;
  const text = promptOf(event, config.botUserId);
  if (isCancelText(text)) {
    const cancel: InboundCancel = {
      kind: "cancel",
      provider: "slack",
      eventId,
      thread,
      authorId: event.user ?? "",
    };
    return { kind: "event", event: cancel };
  }
  const message: InboundMessage = {
    kind: "message",
    provider: "slack",
    eventId,
    thread,
    authorId: event.user ?? "",
    text,
    replyTo: { channelId: thread.channelId, threadId: thread.threadId },
    stage: "message",
  };
  return { kind: "event", event: message };
}

export async function adaptSlack(
  request: RawRequest,
  config: SlackConfig,
  nowSeconds: number,
): Promise<AdapterResult> {
  await verifySlack(request, config, nowSeconds);
  return parseSlackEvent(parseJson(request), config);
}
