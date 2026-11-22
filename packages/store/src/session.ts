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
  // A `stdio` turn waits for the gateway that hosts its agent. If none claims
  // it, the turn fails rather than leaving the thread silent.
  queued: ["dispatched", "failed", "cancelled"],
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

/**
 * How long a queued turn waits for a gateway before it is spent. A `stdio`
 * turn is run by the process that hosts its agent, so a VM that is stopped
 * leaves the turn queued forever. Five minutes is long enough for a gateway
 * restart and short enough that a person is not left waiting on nothing.
 */
export const GATEWAY_CLAIM_DEADLINE_MS = 5 * 60 * 1000;

/** The error a turn carries when no gateway claimed it in time. */
export const NO_GATEWAY_REASON =
  "no gateway claimed this turn: the runtime that hosts this agent was not connected";

/**
 * Whether a queued turn has waited past its claim deadline.
 *
 * Expiry is derived rather than stored, so the rule is a pure function of the
 * turn and the clock and a test can decide it without waiting.
 */
export function hasExpired(turn: Turn, now: number, deadline = GATEWAY_CLAIM_DEADLINE_MS): boolean {
  return turn.status === "queued" && now - turn.createdAt >= deadline;
}

/**
 * The turn failed because no gateway claimed it, or `undefined` when it is
 * still within its deadline or has already advanced. The caller stores the
 * returned turn, so a second sweep finds it terminal and does nothing.
 */
export function expireTurn(
  turn: Turn,
  session: Session,
  now: number,
  deadline = GATEWAY_CLAIM_DEADLINE_MS,
): Turn | undefined {
  if (!hasExpired(turn, now, deadline)) return undefined;
  const result = transitionTurn(turn, session, { status: "failed", error: NO_GATEWAY_REASON }, now);
  return result.ok ? result.next : undefined;
}
