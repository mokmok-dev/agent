import { lenient, object, optional, str } from "@agent/protocol";

/**
 * Linear's Agent Session webhook, as its published schema describes it
 * (`AgentSessionEventWebhookPayload`).
 *
 * The fields this product reads are the session, its issue, the activity that
 * carries a prompt, and the actor. Everything else Linear sends, including the
 * `promptContext` prose and the previous comments, is dropped by the decoder:
 * a turn needs the issue to answer into and the words to answer.
 */

/** A prompt carried either as `body` (an activity) or `prompt` (an event). */
const carrier = object({
  type: optional(str),
  body: optional(lenient(str)),
  prompt: optional(lenient(str)),
});

/** The issue a session answers into. Linear nests it under `agentSession`. */
const issue = object({
  id: str,
  identifier: optional(str),
  title: optional(str),
  teamId: optional(str),
});

/**
 * A Linear agent-session webhook.
 *
 * `agentSession` is required by Linear's schema, but the decoder only requires
 * its `id`, so a payload from an older webhook version still reads.
 */
export const decodeLinearEvent = object({
  type: optional(str),
  action: optional(str),
  webhookId: optional(str),
  organizationId: optional(str),
  appUserId: optional(str),
  promptContext: optional(lenient(str)),
  agentSession: optional(
    object({
      id: str,
      issue: optional(issue),
      commentId: optional(str),
      creatorId: optional(str),
    }),
  ),
  /** Present on `prompted` events, and the prompt on `created` ones. */
  agentActivity: optional(lenient(carrier)),
  actor: optional(object({ id: str, type: optional(str) })),
  user: optional(object({ id: str })),
  /** Older payloads carried the prompt and issue at the top level. */
  issue: optional(issue),
  prompt: optional(lenient(str)),
  body: optional(lenient(str)),
});

/** The event a decode returns when it succeeds. */
export type LinearEvent = Extract<
  ReturnType<typeof decodeLinearEvent>,
  { kind: "decoded" }
>["value"];

/**
 * The issue a session belongs to. Linear puts it under `agentSession`; a payload
 * that states it at the top level is still read, because the adapter has no way
 * to know which webhook version produced it.
 */
export function issueOf(event: LinearEvent): { id: string; identifier?: string } | undefined {
  const nested = event.agentSession?.issue;
  if (nested !== undefined) return nested;
  return event.issue;
}

/**
 * The prompt a turn answers.
 *
 * A `prompted` event carries the person's message in `agentActivity.body`. A
 * `created` event has none, so Linear's `promptContext` is the task: it is the
 * issue, its comments, and the workspace guidance in one formatted string.
 */
export function promptOf(event: LinearEvent): string | undefined {
  const candidates = [
    event.agentActivity?.body,
    event.agentActivity?.prompt,
    event.prompt,
    event.body,
    event.promptContext,
  ];
  for (const candidate of candidates) {
    const trimmed = candidate?.trim();
    if (trimmed !== undefined && trimmed !== "") return trimmed;
  }
  return undefined;
}

export function authorOf(event: LinearEvent): string {
  if (event.actor !== undefined) return event.actor.id;
  if (event.agentSession?.creatorId !== undefined) return event.agentSession.creatorId;
  return event.user === undefined ? "" : event.user.id;
}
