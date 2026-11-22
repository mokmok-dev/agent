import { lenient, object, optional, str } from "@agent/protocol";

/** A prompt carried either as `body` (an activity) or `prompt` (an event). */
const carrier = object({
  type: optional(str),
  body: optional(lenient(str)),
  prompt: optional(lenient(str)),
});

/**
 * A Linear agent-session webhook. The product reads the session, the issue,
 * and the prompt; the rest of Linear's payload is dropped by the decoder.
 */
export const decodeLinearEvent = object({
  type: optional(str),
  action: optional(str),
  webhookId: optional(str),
  organizationId: optional(str),
  agentSession: optional(object({ id: str })),
  issue: optional(object({ id: str, teamId: optional(str) })),
  actor: optional(object({ id: str })),
  user: optional(object({ id: str })),
  agentActivity: optional(lenient(carrier)),
  prompt: optional(lenient(str)),
  body: optional(lenient(str)),
});

export type LinearEvent = ReturnType<typeof decodeLinearEvent>;

export function promptOf(event: LinearEvent): string | undefined {
  const candidates = [
    event.agentActivity?.body,
    event.agentActivity?.prompt,
    event.prompt,
    event.body,
  ];
  for (const candidate of candidates) {
    const trimmed = candidate?.trim();
    if (trimmed !== undefined && trimmed !== "") return trimmed;
  }
  return undefined;
}

export function authorOf(event: LinearEvent): string {
  if (event.actor !== undefined) return event.actor.id;
  return event.user === undefined ? "" : event.user.id;
}
