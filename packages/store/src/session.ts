export type SessionStatus = "active" | "closed";

export type TurnStatus = "queued" | "dispatched" | "running" | "completed" | "failed" | "cancelled";

export type Provider = "slack" | "discord" | "linear";

export type ThreadRef = {
  provider: Provider;
  /** Workspace, guild, or organization id. */
  workspaceId: string;
  channelId: string;
  /** The thread or conversation id. Empty when a top-level message starts the thread. */
  threadId: string;
};

export type Session = {
  id: string;
  thread: ThreadRef;
  status: SessionStatus;
  /** The ACP session id the VM gateway reports after `session/new`. */
  acpSessionId: string | null;
  gatewayId: string | null;
  /**
   * The agent, model, and runtime this session runs on, as the capability
   * router chose them. `null` until the first delivery is routed, so the
   * choice is inspectable after the fact.
   */
  selection: SessionSelection | null;
  /** The pi conversation that holds this session's transcript. */
  conversationId: string | null;
  /** How many turns this session has created; the next turn id continues from here. */
  turnSeq: number;
  createdAt: number;
  updatedAt: number;
};

/**
 * The router's answer, as the session keeps it. It mirrors the capability
 * package's `Selection`, kept structural so the store does not depend on it.
 */
export type SessionSelection = {
  agent: string;
  runtime: string;
  model: { provider: string; modelId: string };
  protocol: string;
};

export type Turn = {
  id: string;
  sessionId: string;
  /** The text the user sent, already normalized by the adapter. */
  prompt: string;
  status: TurnStatus;
  /** The provider's event id: a replay with the same key never creates a second turn. */
  idempotencyKey: string;
  stopReason: string | null;
  error: string | null;
  createdAt: number;
  updatedAt: number;
};

export const TURN_TRANSITIONS: Record<TurnStatus, readonly TurnStatus[]> = {
  queued: ["dispatched", "cancelled"],
  // A dispatch may fail before the agent ever runs, so `failed` is legal here.
  dispatched: ["running", "failed", "cancelled", "queued"],
  running: ["completed", "failed", "cancelled"],
  completed: [],
  failed: [],
  cancelled: [],
};

export const TERMINAL_TURN_STATUSES: readonly TurnStatus[] = ["completed", "failed", "cancelled"];

export function isTerminal(status: TurnStatus): boolean {
  return TURN_TRANSITIONS[status].length === 0;
}

/** The identity of a thread, as a stable string the Durable Object can key on. */
export function threadKey(thread: ThreadRef): string {
  return [thread.provider, thread.workspaceId, thread.channelId, thread.threadId].join(":");
}

export type TransitionRejection = "illegal-transition" | "session-closed" | "already-terminal";

export type TransitionResult<S> =
  { ok: true; next: S } | { ok: false; reason: TransitionRejection };

export type TurnUpdate =
  | { status: "dispatched" }
  | { status: "running" }
  | { status: "queued" }
  | { status: "completed"; stopReason: string }
  | { status: "failed"; error: string }
  | { status: "cancelled" };

/**
 * Move a turn to `update.status`. The transition must be listed in
 * `TURN_TRANSITIONS`, and a turn in a closed session cannot advance. The
 * result carries the next turn, so the caller stores what the rule returned
 * rather than rebuilding the turn itself.
 */
export function transitionTurn(
  turn: Turn,
  session: Session,
  update: TurnUpdate,
  at = 0,
): TransitionResult<Turn> {
  if (isTerminal(turn.status)) return { ok: false, reason: "already-terminal" };
  if (session.status === "closed") return { ok: false, reason: "session-closed" };
  if (!TURN_TRANSITIONS[turn.status].includes(update.status)) {
    return { ok: false, reason: "illegal-transition" };
  }
  return { ok: true, next: applyUpdate(turn, update, at) };
}

function applyUpdate(turn: Turn, update: TurnUpdate, at: number): Turn {
  const base: Turn = { ...turn, status: update.status, updatedAt: at };
  switch (update.status) {
    case "completed":
      return { ...base, stopReason: update.stopReason, error: null };
    case "failed":
      return { ...base, stopReason: null, error: update.error };
    case "cancelled":
      return { ...base, stopReason: null, error: null };
    default:
      return base;
  }
}

/**
 * The next turn to dispatch: the oldest queued one, and the first of its
 * timestamp when two share one. Turns are dispatched in arrival order so a
 * conversation reads in order after a redelivery.
 */
export function nextQueued(turns: readonly Turn[]): Turn | undefined {
  let best: Turn | undefined;
  for (const turn of turns) {
    if (turn.status !== "queued") continue;
    const older = best === undefined || turn.createdAt < best.createdAt;
    const sameAge = best !== undefined && turn.createdAt === best.createdAt && turn.id < best.id;
    if (older || sameAge) best = turn;
  }
  return best;
}

export function acceptsTurns(session: Session): boolean {
  return session.status === "active";
}
