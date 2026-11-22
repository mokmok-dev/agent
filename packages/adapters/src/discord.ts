import type {
  AdapterResult,
  InboundCancel,
  InboundMessage,
  RawRequest,
  ThreadId,
} from "./events.ts";
import { decoded, jsonOf, refuse, refuseSignature } from "./events.ts";
import { isCancelText } from "./commands.ts";
import { headerOf, verifyEd25519, type Verified } from "./signature.ts";
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
 * the raw body, keyed by the application's public key. A refusal names the
 * header or the signature that failed.
 */
export async function verifyDiscord(request: RawRequest, config: DiscordConfig): Promise<Verified> {
  const signature = headerOf(request, "x-signature-ed25519");
  if (signature.kind !== "present") return signature;
  const timestamp = headerOf(request, "x-signature-timestamp");
  if (timestamp.kind !== "present") return timestamp;
  return verifyEd25519(config.publicKey, signature.value, `${timestamp.value}${request.body}`);
}

export function parseDiscordInteraction(body: unknown): AdapterResult {
  return decoded(decodeDiscordInteraction(body), (interaction) => {
    if (interaction.type === DiscordInteractionType.Ping) {
      return { kind: "challenge", response: "pong" };
    }
    const text = promptOf(interaction);
    if (text === undefined) return refuse(400, "Discord interaction has no prompt");
    const eventId = interaction.id;
    if (eventId === undefined) return refuse(400, "Discord interaction has no id");
    const channelId = channelOf(interaction);
    if (channelId === undefined) return refuse(400, "Discord interaction has no channel");
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
      stage: "message",
    };
    return { kind: "event", event: message };
  });
}

export async function adaptDiscord(
  request: RawRequest,
  config: DiscordConfig,
): Promise<AdapterResult> {
  const verified = await verifyDiscord(request, config);
  if (verified.kind !== "verified") return refuseSignature(verified);
  return decoded(jsonOf(request), parseDiscordInteraction);
}

/** The deferred acknowledgement Discord expects for an interaction that will take time. */
export function deferredAck(): Response {
  return Response.json({ type: 5 });
}
