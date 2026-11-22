/** One turn's work, in the shape the control plane already records. */
export type TurnInput = {
  sessionId: string;
  /** The conversation the agent keeps its history in. */
  conversationId: string;
  prompt: string;
  /** The idempotency key, so a redelivered prompt is one turn. */
  operationId: string;
  /** The model the router chose for this session. */
  model: { provider: string; modelId: string };
  now: number;
};

export type TurnResult =
  { kind: "answered"; text: string } | { kind: "unanswered"; reason: string };

/**
 * Runs one turn. This is the seam the control plane knows an agent through, so
 * a test drives the real turn lifecycle with a scripted runner and the
 * platform binds pi behind it.
 */
export interface TurnRunner {
  run(input: TurnInput): Promise<TurnResult>;
}

/** A runner that refuses every turn, for a deployment with no agent configured. */
export function unavailableRunner(reason: string): TurnRunner {
  return { run: async () => ({ kind: "unanswered", reason }) };
}
