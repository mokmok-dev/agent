import {
  acceptsTurns,
  SessionRepository,
  type AppendResult,
  type Session,
  type Storage,
  type ThreadRef,
  type Turn,
  type TurnUpdate,
} from "@agent/store";

/**
 * What a caller can ask a session to do. Every method is a named operation so
 * a Durable Object RPC call reads as one intent, and the caller cannot reach
 * the storage directly.
 */
export type DeliveryOutcome =
  | { kind: "accepted"; session: Session; turn: Turn }
  | { kind: "replayed"; session: Session; turn: Turn }
  | { kind: "refused"; reason: "closed" };

/**
 * One conversation. Calls arrive serialized per object, so two webhooks for
 * one thread cannot interleave a read and a write.
 */
export class SessionService {
  readonly #repo: SessionRepository;

  constructor(storage: Storage) {
    this.#repo = new SessionRepository(storage);
  }

  /** Accept a delivery and queue its turn, or return the turn a replay already created. */
  async deliver(
    sessionId: string,
    thread: ThreadRef,
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<DeliveryOutcome> {
    const session = await this.#repo.ensureSession(sessionId, thread, now);
    if (!acceptsTurns(session)) return { kind: "refused", reason: "closed" };
    const appended: AppendResult = await this.#repo.appendTurn(
      sessionId,
      prompt,
      idempotencyKey,
      now,
    );
    const outcome: DeliveryOutcome =
      appended.kind === "appended"
        ? { kind: "accepted", session, turn: appended.turn }
        : { kind: "replayed", session, turn: appended.turn };
    return outcome;
  }

  session(sessionId: string): Promise<Session | undefined> {
    return this.#repo.session(sessionId);
  }

  turns(sessionId: string): Promise<Turn[]> {
    return this.#repo.turns(sessionId);
  }

  next(sessionId: string): Promise<Turn | undefined> {
    return this.#repo.nextTurn(sessionId);
  }

  active(sessionId: string): Promise<Turn | undefined> {
    return this.#repo.activeTurn(sessionId);
  }

  transition(sessionId: string, turnId: string, update: TurnUpdate, now: number) {
    return this.#repo.transition(sessionId, turnId, update, now);
  }

  /** Record the ACP session the gateway reports. A missing session is the caller's error. */
  bind(sessionId: string, acpSessionId: string, gatewayId: string, now: number) {
    return this.#repo.bindAgentSession(sessionId, acpSessionId, gatewayId, now);
  }

  close(sessionId: string, now: number) {
    return this.#repo.close(sessionId, now);
  }
}
