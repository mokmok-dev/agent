import type {
  AdapterResult,
  InboundCancel,
  InboundMessage,
  RawRequest,
  ThreadId,
} from "./events.ts";
import { decoded, jsonOf, refuse, refuseSignature } from "./events.ts";
import { isCancelText } from "./commands.ts";
import { constantTimeEqual, headerOf, signSha256, type Verified } from "./signature.ts";
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

/**
 * Verify a Slack request: a `v0` HMAC over `v0:<timestamp>:<body>`, and a
 * timestamp inside the skew window so a captured request cannot be replayed
 * later.
 */
export async function verifySlack(
  request: RawRequest,
  config: SlackConfig,
  nowSeconds: number,
): Promise<Verified> {
  const stamp = headerOf(request, "x-slack-request-timestamp");
  const signature = headerOf(request, "x-slack-signature");
  if (stamp.kind !== "present") return stamp;
  if (signature.kind !== "present") return signature;
  const seconds = Number(stamp.value);
  if (!Number.isFinite(seconds)) return { kind: "mismatch" };
  if (Math.abs(nowSeconds - seconds) > SLACK_MAX_SKEW_SECONDS) return { kind: "mismatch" };
  const expected = await signSha256(config.signingSecret, `v0:${stamp.value}:${request.body}`);
  if (expected.kind !== "signed") return expected;
  return constantTimeEqual(`v0=${expected.hex}`, signature.value)
    ? { kind: "verified" }
    : { kind: "mismatch" };
}

export function parseSlackEvent(body: unknown, config: SlackConfig): AdapterResult {
  return decoded(decodeSlackEnvelope(body), (envelope) => {
    if (envelope.type === "url_verification") {
      if (envelope.challenge === undefined) return refuse(400, "missing Slack challenge");
      return { kind: "challenge", response: envelope.challenge };
    }
    if (envelope.payload !== undefined) {
      return { kind: "ignored", reason: "Slack interactive payload" };
    }
    const event = envelope.event;
    if (event === undefined) return refuse(400, "missing Slack event");
    if (event.type === "app_mention") return mentionEvent(envelope, event, config);
    return messageEvent(envelope, event, config);
  });
}

function mentionEvent(
  envelope: SlackEnvelope,
  event: SlackMessage,
  config: SlackConfig,
): AdapterResult {
  const thread = threadOf(event);
  if (thread === undefined) {
    return refuse(400, "Slack mention is missing team, channel, or timestamp");
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
  if (event.ts === undefined) return refuse(400, "Slack message has no timestamp");
  if (event.ts === event.thread_ts) return { kind: "ignored", reason: "thread parent" };
  if (event.user === config.botUserId) return { kind: "ignored", reason: "bot message" };
  const thread = threadOf(event);
  if (thread === undefined) {
    return refuse(400, "Slack message is missing team, channel, or timestamp");
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
  const verified = await verifySlack(request, config, nowSeconds);
  if (verified.kind !== "verified") return refuseSignature(verified);
  return decoded(jsonOf(request), (body) => parseSlackEvent(body, config));
}
