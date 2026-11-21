/**
 * The domain model for a conversation: what a session is, what a turn is, and
 * which transitions are legal. The Durable Object persists this model; the
 * rules here are pure so they can be tested without a runtime.
 */

/** A session is one agent conversation bound to one external thread. */
export type SessionStatus = "active" | "closed";

/** A turn is one user message and the agent's answer to it. */
export type TurnStatus = "queued" | "dispatched" | "running" | "completed" | "failed" | "cancelled";

/** Where a conversation comes from. The adapter that parsed the webhook owns this value. */
export type Provider = "slack" | "discord" | "linear";

/** The external conversation a session is bound to. */
export type ThreadRef = {
  provider: Provider;
  /** Workspace, guild, or organization id. */
  workspaceId: string;
  /** Channel, or the provider's equivalent. */
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
  /** The gateway instance that currently owns this session. */
  gatewayId: string | null;
  createdAt: number;
  updatedAt: number;
};

export type Turn = {
  id: string;
  sessionId: string;
  /** The text the user sent, already normalized by the adapter. */
  prompt: string;
  status: TurnStatus;
  /** The provider's event id: a replay with the same key never creates a second turn. */
  idempotencyKey: string;
  /** Why the turn ended. Set for `completed`. */
  stopReason: string | null;
  /** The failure, set for `failed`. */
  error: string | null;
  createdAt: number;
  updatedAt: number;
};

/** Which statuses a turn may move to from each status. */
export const TURN_TRANSITIONS: Record<TurnStatus, readonly TurnStatus[]> = {
  queued: ["dispatched", "cancelled"],
  dispatched: ["running", "cancelled", "queued"],
  running: ["completed", "failed", "cancelled"],
  completed: [],
  failed: [],
  cancelled: [],
};

/** A status a turn can never leave. Reaching one means the turn is done. */
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

/** What a caller supplies when moving a turn. Only fields the target status needs are read. */
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

/** Whether a new turn may be accepted: a closed session accepts nothing. */
export function acceptsTurns(session: Session): boolean {
  return session.status === "active";
}
