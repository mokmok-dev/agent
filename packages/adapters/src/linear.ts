import type { AdapterResult, InboundMessage, RawRequest } from "./events.ts";
import { decoded, jsonOf, refuse, refuseSignature } from "./events.ts";
import { headerOf, verifyHmac, type Verified } from "./signature.ts";
import { authorOf, decodeLinearEvent, issueOf, promptOf } from "./linear-schema.ts";

export type LinearConfig = { webhookSecret: string };

/**
 * Verify a Linear webhook: `Linear-Signature` is the hex HMAC-SHA256 of the
 * raw body, with the webhook secret as the key.
 */
export async function verifyLinear(request: RawRequest, config: LinearConfig): Promise<Verified> {
  const signature = headerOf(request, "linear-signature");
  if (signature.kind !== "present") return signature;
  return verifyHmac(config.webhookSecret, request.body, signature.value, "sha256");
}

/**
 * Read one Linear delivery.
 *
 * Only an `AgentSessionEvent` opens a turn. Linear also sends `Issue` and
 * `Comment` events for the same resources, and those carry no agent session, so
 * they are ignored by name rather than treated as a failure.
 */
export function parseLinearEvent(body: unknown): AdapterResult {
  return decoded(decodeLinearEvent(body), (event) => {
    if (event.type !== "AgentSessionEvent") {
      return { kind: "ignored", reason: `unsupported Linear event: ${event.type ?? "unknown"}` };
    }
    const session = event.agentSession;
    if (session === undefined) return refuse(400, "Linear event has no agent session");
    // A thought we emitted ourselves comes back as a webhook; answering it would
    // make the agent talk to itself.
    if (event.agentActivity?.type === "thought") {
      return { kind: "ignored", reason: "Linear activity echo" };
    }
    const text = promptOf(event);
    if (text === undefined) return { kind: "ignored", reason: "Linear event has no prompt" };
    // The issue is nested under the session, which is where Linear's published
    // payload puts it; the session id is the fallback thread key.
    const issue = issueOf(event);
    const thread = {
      workspaceId: event.organizationId ?? "",
      channelId: issueIdOf(issue?.id, session.id),
      threadId: session.id,
    };
    const message: InboundMessage = {
      kind: "message",
      provider: "linear",
      eventId: event.webhookId ?? session.id,
      thread,
      authorId: authorOf(event),
      text,
      replyTo: { channelId: issueIdOf(issue?.id, session.id), threadId: session.id },
      stage: event.action === "prompted" ? "prompt" : "start",
    };
    return { kind: "event", event: message };
  });
}

/**
 * The issue a session belongs to, or the session id when Linear sends no issue.
 * A `prompted` event carries the session and no issue, so the session id is the
 * only stable thread key in that case.
 */
function issueIdOf(issueId: string | undefined, sessionId: string): string {
  return issueId === undefined || issueId === "" ? sessionId : issueId;
}

export async function adaptLinear(
  request: RawRequest,
  config: LinearConfig,
): Promise<AdapterResult> {
  const verified = await verifyLinear(request, config);
  if (verified.kind !== "verified") return refuseSignature(verified);
  return decoded(jsonOf(request), parseLinearEvent);
}
