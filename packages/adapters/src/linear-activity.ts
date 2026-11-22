/**
 * Linear's agent activity, which is how an agent tells a person what it is
 * doing inside a session. Linear validates the shape of each type on its side,
 * so the type and its fields are a closed set here.
 */

export const ACTIVITY_TYPES = ["thought", "action", "elicitation", "response", "error"] as const;

export type ActivityType = (typeof ACTIVITY_TYPES)[number];

/** The `content` of an activity. Each type carries only the fields Linear accepts. */
export type ActivityContent =
  | { type: "thought"; body: string }
  | { type: "elicitation"; body: string }
  | { type: "response"; body: string }
  | { type: "error"; body: string }
  | { type: "action"; action: string; parameter: string; result?: string };

export type AgentActivityInput = {
  agentSessionId: string;
  content: ActivityContent;
};

/** A thought, which Linear asks an agent to send within ten seconds of a session. */
export function thought(body: string): AgentActivityInput["content"] {
  return { type: "thought", body };
}

/** A final answer for the session. */
export function response(body: string): AgentActivityInput["content"] {
  return { type: "response", body };
}

/** A failure the person should see, with whatever the agent can say about it. */
export function failure(body: string): AgentActivityInput["content"] {
  return { type: "error", body };
}

/** One tool call, optionally with its result once the call has finished. */
export function action(
  action: string,
  parameter: string,
  result?: string,
): AgentActivityInput["content"] {
  return result === undefined
    ? { type: "action", action, parameter }
    : { type: "action", action, parameter, result };
}

export const AGENT_ACTIVITY_CREATE = `mutation AgentActivityCreate($input: AgentActivityCreateInput!) {
  agentActivityCreate(input: $input) {
    success
    agentActivity {
      id
    }
  }
}`;

/** The variables Linear's `agentActivityCreate` mutation takes. */
export function activityVariables(input: AgentActivityInput): {
  input: AgentActivityInput;
} {
  return { input };
}

export type ActivityResult = { ok: true; activityId: string | null } | { ok: false; error: string };

/**
 * Read Linear's answer. A `success: false` and a GraphQL error are both
 * refusals; the first is Linear declining this activity, the second is a shape
 * or auth problem that its message explains.
 */
export function readActivityResult(body: unknown): ActivityResult {
  const root = (body ?? {}) as {
    errors?: unknown;
    data?: { agentActivityCreate?: { success?: unknown; agentActivity?: unknown } };
  };
  const errors = root.errors;
  if (Array.isArray(errors) && errors.length > 0) {
    const first = (errors[0] as { message?: unknown } | null)?.message;
    return { ok: false, error: typeof first === "string" ? first : "linear rejected the request" };
  }
  const data = root.data;
  const payload = data === undefined ? undefined : data.agentActivityCreate;
  if (payload === undefined) return { ok: false, error: "linear answered without an activity" };
  if (payload.success !== true) return { ok: false, error: "linear declined the activity" };
  const activity = payload.agentActivity as { id?: unknown } | null | undefined;
  if (activity === null || activity === undefined) return { ok: true, activityId: null };
  return { ok: true, activityId: typeof activity.id === "string" ? activity.id : null };
}

/** A GraphQL call carrying a bearer token. */
export type ActivityRequest = (
  url: string,
  init: { method: string; headers: Record<string, string>; body: string },
) => Promise<unknown>;

/**
 * Send one activity. A refusal is returned, not thrown, so a failed reply can be
 * reported without losing the turn that produced it.
 */
export async function sendActivity(
  accessToken: string,
  input: AgentActivityInput,
  request: ActivityRequest,
): Promise<ActivityResult> {
  let body: unknown;
  try {
    body = await request("https://api.linear.app/graphql", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${accessToken}` },
      body: JSON.stringify({
        query: AGENT_ACTIVITY_CREATE,
        variables: activityVariables(input),
      }),
    });
  } catch (error) {
    return { ok: false, error: `linear unreachable: ${String(error)}` };
  }
  return readActivityResult(body);
}
