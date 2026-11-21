import type { AdapterResult, InboundMessage, RawRequest } from "./events.ts";
import { AdapterError, decodeOr400 } from "./events.ts";
import { parseJson } from "./slack.ts";
import { verifyHmac } from "./signature.ts";
import { authorOf, decodeLinearEvent, promptOf } from "./linear-schema.ts";

export type LinearConfig = { webhookSecret: string };

/**
 * Verify a Linear webhook: `Linear-Signature` is the hex HMAC-SHA256 of the
 * raw body, with the webhook secret as the key.
 */
export async function verifyLinear(request: RawRequest, config: LinearConfig): Promise<void> {
  const signature = request.headers["linear-signature"];
  if (signature === undefined) throw new AdapterError(401, "missing Linear signature");
  await verifyHmac(config.webhookSecret, request.body, signature, "sha256");
}

export function parseLinearEvent(body: unknown): AdapterResult {
  const event = decodeOr400(decodeLinearEvent, body);
  if (event.type !== "AgentSessionEvent") {
    return { kind: "ignored", reason: `unsupported Linear event: ${event.type ?? "unknown"}` };
  }
  const session = event.agentSession;
  if (session === undefined) throw new AdapterError(400, "Linear event has no agent session");
  const issue = event.issue;
  if (issue === undefined) throw new AdapterError(400, "Linear event has no issue");
  const teamId = issue.teamId;
  if (teamId === undefined) throw new AdapterError(400, "Linear event has no team");
  if (event.agentActivity?.type === "thought") {
    return { kind: "ignored", reason: "Linear activity echo" };
  }
  const text = promptOf(event);
  if (text === undefined) return { kind: "ignored", reason: "Linear event has no prompt" };
  const thread = { workspaceId: teamId, channelId: issue.id, threadId: session.id };
  const message: InboundMessage = {
    kind: "message",
    provider: "linear",
    eventId: event.webhookId ?? session.id,
    thread,
    authorId: authorOf(event),
    text,
    replyTo: { channelId: issue.id, threadId: session.id },
  };
  return { kind: "event", event: message };
}

export async function adaptLinear(
  request: RawRequest,
  config: LinearConfig,
): Promise<AdapterResult> {
  await verifyLinear(request, config);
  return parseLinearEvent(parseJson(request));
}
