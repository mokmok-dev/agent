import { lenient, object, optional, record, str } from "@agent/protocol";
import type { ThreadId } from "./events.ts";

/** One Slack event. `type` identifies it; everything the adapter reads is optional. */
const slackEvent = object({
  type: str,
  team: optional(str),
  channel: optional(str),
  ts: optional(str),
  thread_ts: optional(str),
  user: optional(str),
  // A non-string `text` is treated as absent, so a malformed body still parses.
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

export type SlackEnvelope = ReturnType<typeof decodeSlackEnvelope>;

/** A Slack message event, narrowed to the shape the adapter acts on. */
export type SlackMessage = NonNullable<SlackEnvelope["event"]>;

/** The thread a Slack message belongs to. A mention starts a thread at its own timestamp. */
export function threadOf(event: SlackMessage): ThreadId | undefined {
  const { team, channel, ts } = event;
  if (team === undefined || channel === undefined || ts === undefined) return undefined;
  const parent = event.thread_ts ?? ts;
  return { workspaceId: team, channelId: channel, threadId: parent };
}

/** Remove `<@BOTID>` from the text and collapse the space it leaves behind. */
export function stripMention(text: string, botUserId: string): string {
  return text.split(`<@${botUserId}>`).join(" ").replace(/\s+/g, " ").trim();
}

/** The text of a message, with the agent's mention removed. Missing text is an empty prompt. */
export function promptOf(event: SlackMessage, botUserId: string): string {
  return stripMention(event.text ?? "", botUserId);
}
