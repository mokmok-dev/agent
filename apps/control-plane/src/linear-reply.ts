import {
  response,
  sendActivity,
  thought,
  type ActivityContent,
  type ActivityRequest,
} from "@agent/adapters";
import type { LinearOAuthPort } from "./linear-oauth-port.ts";

/**
 * What the control plane says back to Linear. A session needs a thought within
 * ten seconds of opening, and a response when the work is done, so those are the
 * two things this port offers.
 */
export interface LinearReplyPort {
  /** Say that work has started. Linear marks the session responsive on receipt. */
  acknowledge(sessionId: string, now: number): Promise<ReplyOutcome>;
  /** Deliver the agent's answer. */
  answer(sessionId: string, text: string, now: number): Promise<ReplyOutcome>;
}

export type ReplyOutcome =
  | { kind: "sent"; activityId: string | null }
  | { kind: "skipped"; reason: "no-credential" }
  | { kind: "failed"; error: string };

/**
 * A reply port over the stored Linear credential. A missing credential is
 * reported rather than thrown, because a deployment without a login should keep
 * accepting webhooks and record turns it cannot answer.
 */
export function linearReplies(
  oauth: LinearOAuthPort,
  request: ActivityRequest = postActivity,
  endpoint?: string,
): LinearReplyPort {
  const send = async (
    sessionId: string,
    content: ActivityContent,
    now: number,
  ): Promise<ReplyOutcome> => {
    const access = await oauth.accessToken(now);
    if (access === undefined) return { kind: "skipped", reason: "no-credential" };
    if (access.kind !== "token") return { kind: "failed", error: access.message };
    const result = await sendActivity(
      access.token,
      { agentSessionId: sessionId, content },
      request,
      endpoint,
    );
    return result.kind === "sent"
      ? { kind: "sent", activityId: result.activityId }
      : { kind: "failed", error: result.error };
  };
  return {
    acknowledge: (sessionId, now) => send(sessionId, thought("Working on it."), now),
    answer: (sessionId, text, now) => send(sessionId, response(text), now),
  };
}

export async function postActivity(
  url: string,
  init: { method: string; headers: Record<string, string>; body: string },
): Promise<unknown> {
  const result = await fetch(url, init);
  return result.json();
}
