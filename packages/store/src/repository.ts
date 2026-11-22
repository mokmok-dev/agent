import {
  expireTurn,
  nextQueued,
  transitionTurn,
  type AgentCapability,
  type Session,
  type SessionSelection,
  type ThreadRef,
  type Turn,
  type TurnUpdate,
} from "./session.ts";

export interface Storage {
  get<T>(key: string): Promise<T | undefined>;
  put<T>(key: string, value: T): Promise<void>;
  delete(key: string): Promise<void>;
  list<T>(prefix: string): Promise<Array<{ key: string; value: T }>>;
}

export type AppendResult = { kind: "appended"; turn: Turn } | { kind: "replayed"; turn: Turn };

export type TransitionOutcome =
  | { ok: true; next: Turn }
  | {
      ok: false;
      reason: "not-found" | "illegal-transition" | "session-closed" | "already-terminal";
    };

class MissingSession extends Error {
  constructor(readonly sessionId: string) {
    super(`no session ${sessionId}`);
    this.name = "MissingSession";
  }
}

/**
 * The session and turn records, keyed for a Durable Object's single-object
 * storage. Every write derives from what is stored now, so replaying a
 * delivery converges on the same state.
 */
export class SessionRepository {
  readonly #storage: Storage;

  constructor(storage: Storage) {
    this.#storage = storage;
  }

  async createSession(id: string, thread: ThreadRef, now: number): Promise<Session> {
    const session: Session = {
      id,
      thread,
      status: "active",
      acpSessionId: null,
      gatewayId: null,
      selection: null,
      capability: null,
      conversationId: null,
      turnSeq: 0,
      createdAt: now,
      updatedAt: now,
    };
    await this.#storage.put(sessionKey(id), session);
    return session;
  }

  async ensureSession(id: string, thread: ThreadRef, now: number): Promise<Session> {
    const existing = await this.session(id);
    return existing ?? (await this.createSession(id, thread, now));
  }

  session(id: string): Promise<Session | undefined> {
    return this.#storage.get<Session>(sessionKey(id));
  }

  turns(sessionId: string): Promise<Turn[]> {
    return this.#storage
      .list<Turn>(turnPrefix(sessionId))
      .then((entries) => entries.map((entry) => entry.value));
  }

  /**
   * Record a turn for a delivery. The idempotency key is the provider's event
   * id, and the turn is stored under that key as well, so a redelivered
   * webhook finds the turn it already created in one read.
   */
  async appendTurn(
    sessionId: string,
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<AppendResult> {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const seen = await this.#storage.get<Turn>(idemKey(sessionId, idempotencyKey));
    if (seen !== undefined) return { kind: "replayed", turn: seen };
    const seq = session.turnSeq + 1;
    const turn: Turn = {
      id: `t${seq}`,
      sessionId,
      prompt,
      status: "queued",
      idempotencyKey,
      stopReason: null,
      error: null,
      createdAt: now,
      updatedAt: now,
    };
    await this.#storage.put(turnKey(sessionId, turn.id), turn);
    await this.#storage.put(idemKey(sessionId, idempotencyKey), turn);
    await this.#storage.put(sessionKey(sessionId), { ...session, turnSeq: seq, updatedAt: now });
    return { kind: "appended", turn };
  }

  async nextTurn(sessionId: string): Promise<Turn | undefined> {
    return nextQueued(await this.turns(sessionId));
  }

  /**
   * The oldest queued turn together with the session that owns it. The two come
   * from one read, so a caller cannot see a turn whose route it then has to
   * look up and hope exists.
   */
  async nextWork(sessionId: string): Promise<{ turn: Turn; session: Session } | undefined> {
    const session = await this.session(sessionId);
    if (session === undefined) return undefined;
    const turn = nextQueued(await this.turns(sessionId));
    return turn === undefined ? undefined : { turn, session };
  }

  /**
   * Fail every queued turn that waited past its claim deadline, and return the
   * ones that changed.
   *
   * A `stdio` turn is run by the gateway that hosts its agent, so a stopped VM
   * would otherwise leave it queued forever and the thread silent. Running the
   * sweep on the same object that owns the turns keeps it to one storage read
   * per turn and makes it idempotent: an already terminal turn is skipped.
   */
  async expireStaleTurns(sessionId: string, now: number): Promise<Turn[]> {
    return this.#serialized(async () => {
      const session = await this.session(sessionId);
      if (session === undefined) return [];
      const expired: Turn[] = [];
      for (const turn of await this.turns(sessionId)) {
        const next = expireTurn(turn, session, now);
        if (next === undefined) continue;
        await this.#storage.put(turnKey(sessionId, turn.id), next);
        expired.push(next);
      }
      return expired;
    });
  }

  async activeTurn(sessionId: string): Promise<Turn | undefined> {
    const turns = await this.turns(sessionId);
    const live = turns.find((turn) => turn.status === "running" || turn.status === "dispatched");
    return live ?? nextQueued(turns);
  }

  /**
   * Move a turn through the lifecycle. The stored session decides whether the
   * move is legal, and a rejected move leaves storage untouched.
   */
  async transition(
    sessionId: string,
    turnId: string,
    update: TurnUpdate,
    now: number,
  ): Promise<TransitionOutcome> {
    return this.#serialized(async () => {
      const session = await this.session(sessionId);
      if (session === undefined) throw new MissingSession(sessionId);
      const turn = await this.#storage.get<Turn>(turnKey(sessionId, turnId));
      if (turn === undefined) return { ok: false, reason: "not-found" } as const;
      const result = transitionTurn(turn, session, update, now);
      if (!result.ok) return result;
      await this.#storage.put(turnKey(sessionId, turnId), result.next);
      return result;
    });
  }

  /**
   * Claim a queued turn for one runner. The read and the write happen under one
   * lock, so two callers racing for the same turn cannot both see `queued`; one
   * gets the claim and the other is told it lost.
   */
  async claimTurn(sessionId: string, turnId: string, now: number): Promise<TransitionOutcome> {
    return this.transition(sessionId, turnId, { status: "dispatched" }, now);
  }

  /**
   * Run one read-modify-write at a time. A Durable Object serializes calls to
   * one object, so a queue here is the same guarantee in a test host, and it
   * keeps a claim atomic without a second storage primitive.
   */
  #queue: Promise<unknown> = Promise.resolve();

  #serialized<T>(work: () => Promise<T>): Promise<T> {
    const run = this.#queue.then(work, work);
    this.#queue = run.catch(() => undefined);
    return run;
  }

  async bindAgentSession(sessionId: string, acpSessionId: string, gatewayId: string, now: number) {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const next: Session = { ...session, acpSessionId, gatewayId, updatedAt: now };
    await this.#storage.put(sessionKey(sessionId), next);
    return next;
  }

  /**
   * Record what the session's agent said it can run. It arrives from a process
   * outside this deployment, so it is written only after the gateway reported
   * it, and a later report replaces it: an agent's credentials can change.
   */
  async recordCapability(
    sessionId: string,
    capability: AgentCapability,
    now: number,
  ): Promise<Session> {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const next: Session = { ...session, capability, updatedAt: now };
    await this.#storage.put(sessionKey(sessionId), next);
    return next;
  }

  /**
   * Record the router's choice and the pi conversation it runs in. A session
   * keeps the choice it already made, so a redelivery cannot move a live
   * conversation to another agent or runtime.
   */
  async route(
    sessionId: string,
    selection: SessionSelection,
    conversationId: string,
    now: number,
  ): Promise<Session> {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    if (session.selection !== null) return session;
    const next: Session = { ...session, selection, conversationId, updatedAt: now };
    await this.#storage.put(sessionKey(sessionId), next);
    return next;
  }

  async close(sessionId: string, now: number): Promise<Session> {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const next: Session = { ...session, status: "closed", updatedAt: now };
    await this.#storage.put(sessionKey(sessionId), next);
    return next;
  }
}

export const sessionKey = (id: string): string => `session:${id}`;
export const turnPrefix = (sessionId: string): string => `turn:${sessionId}:`;
export const turnKey = (sessionId: string, turnId: string): string =>
  `${turnPrefix(sessionId)}${turnId}`;
export const idemKey = (sessionId: string, idempotencyKey: string): string =>
  `idem:${sessionId}:${idempotencyKey}`;

export { MissingSession, nextQueued };
