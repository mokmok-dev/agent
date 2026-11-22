import { lenient, object, optional, record, str } from "@agent/protocol";
import type { ThreadId } from "./events.ts";

const slackEvent = object({
  type: str,
  team: optional(str),
  channel: optional(str),
  ts: optional(str),
  thread_ts: optional(str),
  user: optional(str),
  text: optional(lenient(str)),
});

/**
 * The Slack envelopes the adapter reads: an event callback, the
 * `url_verification` handshake, and an interactive payload. Any other
 * envelope still parses, so the adapter can ignore it by name.
 */
export const decodeSlackEnvelope = object({
  type: optional(str),
  challenge: optional(str),
  event_id: optional(str),
  event: optional(lenient(slackEvent)),
  payload: optional(lenient(record)),
});

/** The envelope a decode returns when it succeeds. */
export type SlackEnvelope = Extract<
  ReturnType<typeof decodeSlackEnvelope>,
  { kind: "decoded" }
>["value"];

export type SlackMessage = NonNullable<SlackEnvelope["event"]>;

export function threadOf(event: SlackMessage): ThreadId | undefined {
  const { team, channel, ts } = event;
  if (team === undefined || channel === undefined || ts === undefined) return undefined;
  const parent = event.thread_ts ?? ts;
  return { workspaceId: team, channelId: channel, threadId: parent };
}

export function stripMention(text: string, botUserId: string): string {
  return text.split(`<@${botUserId}>`).join(" ").replace(/\s+/g, " ").trim();
}

export function promptOf(event: SlackMessage, botUserId: string): string {
  return stripMention(event.text ?? "", botUserId);
}
