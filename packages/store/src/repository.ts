import {
  nextQueued,
  transitionTurn,
  type Session,
  type ThreadRef,
  type Turn,
  type TurnUpdate,
} from "./session.ts";

/**
 * The storage the repository needs. A Durable Object supplies `ctx.storage`
 * through an adapter; a test supplies a map. Nothing here depends on the
 * runtime, so every rule is exercised in a plain process.
 */
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

/** The session the writer needs, or a not-found outcome. */
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
      turnSeq: 0,
      createdAt: now,
      updatedAt: now,
    };
    await this.#storage.put(sessionKey(id), session);
    return session;
  }

  /** The session, created on first use. A second call with the same id returns the stored one. */
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

  /** The turn to dispatch next, or `undefined` when nothing is queued. */
  async nextTurn(sessionId: string): Promise<Turn | undefined> {
    return nextQueued(await this.turns(sessionId));
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
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const turn = await this.#storage.get<Turn>(turnKey(sessionId, turnId));
    if (turn === undefined) return { ok: false, reason: "not-found" };
    const result = transitionTurn(turn, session, update, now);
    if (!result.ok) return result;
    await this.#storage.put(turnKey(sessionId, turnId), result.next);
    return result;
  }

  /** Record the ACP session id the gateway reports after `session/new`. */
  async bindAgentSession(sessionId: string, acpSessionId: string, gatewayId: string, now: number) {
    const session = await this.session(sessionId);
    if (session === undefined) throw new MissingSession(sessionId);
    const next: Session = { ...session, acpSessionId, gatewayId, updatedAt: now };
    await this.#storage.put(sessionKey(sessionId), next);
    return next;
  }

  /** Close a session. Every later turn is refused. */
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
