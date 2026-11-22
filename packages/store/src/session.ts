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
  /**
   * What the agent that hosts this session said it can run, once a gateway
   * reported it. `null` until then, which is different from an agent that
   * states no models at all: a `stdio` agent's models depend on its own
   * credentials, so the router's choice is only known to be valid after this.
   */
  capability: AgentCapability | null;
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

/**
 * What a session's agent said it can run, kept structural like the selection so
 * the store does not depend on the protocol package.
 *
 * `statesModels` separates "this agent listed no models" from "no agent has
 * reported yet", which is what lets a caller refuse a routed model only when
 * the agent actually named its list.
 */
export type AgentCapability = {
  agent?: { name: string; version: string };
  currentModelId?: string;
  availableModels: string[];
  statesModels: boolean;
};

/**
 * Why a routed model cannot run on the agent that hosts the session. A `stdio`
 * agent offers whatever its own credentials reach, so the router's choice must
 * be checked against what the agent reported.
 */
export const NO_MODEL_REASON = (reported: AgentCapability, modelId: string): string =>
  `the agent does not offer ${modelId}; it lists ${
    reported.availableModels.length === 0 ? "no models" : reported.availableModels.join(", ")
  }`;

/** Whether the routed model is one the agent said it offers. */
export function offersModel(capability: AgentCapability, modelId: string): boolean {
  return capability.statesModels && capability.availableModels.includes(modelId);
}

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
 * How long a turn that a gateway claimed may stay unsettled before it is
 * treated as abandoned.
 *
 * A gateway that dies mid-turn leaves its turn `running` for ever: nothing
 * settles it, and a gateway asking for the next session skips that session
 * because its turn is not queued. One crash then hides every later delegation
 * on the same thread. The grace is longer than any single turn should take so a
 * slow agent is not failed early.
 */
export const TURN_ABANDONED_DEADLINE_MS = 30 * 60 * 1000;

/** The error a turn carries when the gateway that claimed it stopped reporting. */
export const ABANDONED_TURN_REASON =
  "the gateway that claimed this turn stopped reporting; the run did not finish";

/**
 * Whether a queued turn has waited past its claim deadline.
 *
 * Expiry is derived rather than stored, so the rule is a pure function of the
 * turn and the clock and a test can decide it without waiting.
 */
export function hasExpired(turn: Turn, now: number, deadline = GATEWAY_CLAIM_DEADLINE_MS): boolean {
  if (turn.status === "queued") return now - turn.createdAt >= deadline;
  // A claimed turn is only abandoned after the longer grace, measured from the
  // moment the gateway last reported progress.
  if (turn.status === "dispatched" || turn.status === "running") {
    return now - turn.updatedAt >= TURN_ABANDONED_DEADLINE_MS;
  }
  return false;
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
  // A queued turn never reached a gateway; a claimed one reached it and was
  // abandoned, which is a different thing to tell the thread.
  const error = turn.status === "queued" ? NO_GATEWAY_REASON : ABANDONED_TURN_REASON;
  const result = transitionTurn(turn, session, { status: "failed", error }, now);
  return result.ok ? result.next : undefined;
}

/**
 * When a session next owes a decision about a waiting turn, or `undefined`
 * when nothing is waiting.
 *
 * Expiry is a deadline, so something has to wake up at it. A caller that only
 * swept on a delivery or a gateway connection left an idle session silent: the
 * webhook answered `202`, no gateway ever came, and nothing ever revisited the
 * turn. This is the instant to revisit, so a host can schedule it rather than
 * poll.
 */
export function nextDeadline(
  turns: readonly Turn[],
  now: number,
  deadline = GATEWAY_CLAIM_DEADLINE_MS,
): number | undefined {
  let soonest: number | undefined;
  for (const turn of turns) {
    // Only a queued turn has a deadline. A dispatched or running one belongs to
    // a gateway that claimed it, and failing it would cut off a slow agent.
    if (turn.status !== "queued") continue;
    const at = turn.createdAt + deadline;
    if (soonest === undefined || at < soonest) soonest = at;
  }
  // A deadline already in the past is due now, which is what makes a sweep that
  // ran late still sweep rather than schedule a wake-up in the past.
  return soonest === undefined ? undefined : Math.max(soonest, now);
}
