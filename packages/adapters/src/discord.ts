import type {
  AdapterResult,
  InboundCancel,
  InboundMessage,
  RawRequest,
  ThreadId,
} from "./events.ts";
import { AdapterError, decodeOr400 } from "./events.ts";
import { isCancelText } from "./commands.ts";
import { parseJson } from "./slack.ts";
import { verifyEd25519 } from "./signature.ts";
import {
  authorOf,
  channelOf,
  decodeDiscordInteraction,
  guildOf,
  promptOf,
} from "./discord-schema.ts";

export const DiscordInteractionType = {
  Ping: 1,
  ApplicationCommand: 2,
  MessageComponent: 3,
  ModalSubmit: 5,
} as const;

export type DiscordConfig = { publicKey: string };

/**
 * Verify a Discord interaction: an Ed25519 signature over the timestamp and
 * the raw body, keyed by the application's public key.
 */
export async function verifyDiscord(request: RawRequest, config: DiscordConfig): Promise<void> {
  const signature = request.headers["x-signature-ed25519"];
  const timestamp = request.headers["x-signature-timestamp"];
  if (signature === undefined) throw new AdapterError(401, "missing Discord signature");
  if (timestamp === undefined) throw new AdapterError(401, "missing Discord timestamp");
  await verifyEd25519(config.publicKey, signature, `${timestamp}${request.body}`);
}

/**
 * Turn a Discord interaction into a product event. `PING` needs a `PONG`, and
 * a slash command or a button press becomes a turn in the interaction's
 * channel and thread.
 */
export function parseDiscordInteraction(body: unknown): AdapterResult {
  const interaction = decodeOr400(decodeDiscordInteraction, body);
  if (interaction.type === DiscordInteractionType.Ping)
    return { kind: "challenge", response: "pong" };

  const text = promptOf(interaction);
  if (text === undefined) throw new AdapterError(400, "Discord interaction has no prompt");
  const eventId = interaction.id;
  if (eventId === undefined) throw new AdapterError(400, "Discord interaction has no id");
  const channelId = channelOf(interaction);
  if (channelId === undefined) throw new AdapterError(400, "Discord interaction has no channel");
  const thread = {
    workspaceId: guildOf(interaction),
    channelId,
    threadId: channelId,
  } satisfies ThreadId;

  if (isCancelText(text)) {
    const cancel: InboundCancel = {
      kind: "cancel",
      provider: "discord",
      eventId,
      thread,
      authorId: authorOf(interaction),
    };
    return { kind: "event", event: cancel };
  }
  const message: InboundMessage = {
    kind: "message",
    provider: "discord",
    eventId,
    thread,
    authorId: authorOf(interaction),
    text,
    replyTo: { channelId, threadId: channelId },
  };
  return { kind: "event", event: message };
}

export async function adaptDiscord(
  request: RawRequest,
  config: DiscordConfig,
): Promise<AdapterResult> {
  await verifyDiscord(request, config);
  return parseDiscordInteraction(parseJson(request));
}

/** The deferred acknowledgement Discord expects for an interaction that will take time. */
export function deferredAck(): Response {
  return Response.json({ type: 5 });
}
