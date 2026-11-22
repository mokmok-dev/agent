import { SessionService } from "./session-service.ts";
import { durableStorage, type StorageHost } from "./storage.ts";
import { handleSocketMessage, type GatewaySessions } from "./gateway-connection.ts";
import type { TurnRunOutcome } from "./session-service.ts";
import type { TurnRunner } from "./turn-runner.ts";

/**
 * One object per conversation. It takes a `StorageHost` and its collaborators,
 * so a test drives the real object and the platform entry only adapts the
 * platform to it.
 *
 * Whether a turn runs here or waits for a gateway follows from the route the
 * session chose, not from what the host happens to have: a `durable` protocol
 * runs pi in this process, and a `stdio` protocol records the turn and leaves
 * it queued for the gateway that hosts that agent.
 */
export class SessionObject {
  readonly #service: SessionService;
  readonly #runner: TurnRunner | undefined;
  /** Keeps a detached run alive after `accept` answers, on the platform. */
  readonly #keepAlive: (work: Promise<unknown>) => void;
  /** Answers the thread a settled turn came from. */
  readonly #replies: ReplySink;

  constructor(
    storageHost: StorageHost,
    deps?: ConstructorParameters<typeof SessionService>[1],
    runner?: TurnRunner,
    keepAlive?: (work: Promise<unknown>) => void,
    replies?: ReplySink,
  ) {
    this.#service = new SessionService(durableStorage(storageHost), deps);
    this.#runner = runner;
    this.#keepAlive = keepAlive ?? (() => undefined);
    this.#replies = replies ?? (async () => undefined);
  }

  deliver(...args: Parameters<SessionService["deliver"]>) {
    return this.#service.deliver(...args);
  }

  /**
   * Record a delivery and run its turn when this object hosts that turn's
   * agent. A `stdio` session is left queued for its gateway, which is what
   * makes a VM-routed delivery reach the VM rather than a local pi loop.
   *
   * The run is handed to the host's keep-alive, because Linear requires a
   * webhook answer within five seconds and a model call outlives that. Without
   * it the object hibernates the moment `accept` returns and the run is
   * cancelled mid-flight, which is what left turns with no reply and no error.
   */
  async accept(
    sessionId: string,
    thread: Parameters<SessionService["deliver"]>[1],
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<unknown> {
    const outcome = await this.#service.deliver(sessionId, thread, prompt, idempotencyKey, now);
    if (outcome.kind === "refused") return outcome;
    // A `stdio` route is a process the gateway supervises, so the turn waits.
    if (outcome.selection.protocol !== "stdio") {
      if (this.#runner === undefined) return outcome;
      this.#keepAlive(this.#runDetached(sessionId, now));
      return outcome;
    }
    // A `stdio` turn may be the one that replaces a turn whose gateway never
    // arrived, so the delivery is also the moment to record that failure.
    await this.expire(sessionId, now);
    return outcome;
  }

  /**
   * Fail the queued turns no gateway claimed in time. Called on a delivery and
   * on a gateway connection, which are the two moments a thread can learn that
   * the agent it waits for is not coming.
   */
  async expire(sessionId: string, now: number): Promise<unknown[]> {
    const expired = await this.#service.expire(sessionId, now);
    if (expired.length === 0) return expired;
    const session = await this.#service.session(sessionId);
    if (session === undefined) return expired;
    for (const turn of expired) {
      await this.#replies(
        { kind: "failed", error: turn.error ?? "no gateway claimed this turn" },
        session.thread.threadId,
      );
    }
    return expired;
  }

  /**
   * Run the queued turn without holding the caller, then answer the thread it
   * came from. The reply belongs here rather than in the webhook handler,
   * because the webhook has already answered Linear by the time the run
   * settles; only the run knows whether the agent spoke or failed.
   */
  async #runDetached(sessionId: string, now: number): Promise<void> {
    if (this.#runner === undefined) return;
    try {
      const outcome = await this.#service.runNext(sessionId, this.#runner, now);
      // A run that found no queued turn has nothing to answer with.
      if (outcome === undefined) return;
      const session = await this.#service.session(sessionId);
      const reply = replyOf(outcome);
      // A session that vanished has nowhere to answer.
      if (reply === undefined || session === undefined) return;
      await this.#replies(reply, session.thread.threadId);
    } catch (error) {
      console.log(
        `turn run failed for ${sessionId}: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  session(...args: Parameters<SessionService["session"]>) {
    return this.#service.session(...args);
  }

  turns(...args: Parameters<SessionService["turns"]>) {
    return this.#service.turns(...args);
  }

  next(...args: Parameters<SessionService["next"]>) {
    return this.#service.next(...args);
  }

  active(...args: Parameters<SessionService["active"]>) {
    return this.#service.active(...args);
  }

  transition(...args: Parameters<SessionService["transition"]>) {
    return this.#service.transition(...args);
  }

  bind(...args: Parameters<SessionService["bind"]>) {
    return this.#service.bind(...args);
  }

  capability(...args: Parameters<SessionService["capability"]>) {
    return this.#service.capability(...args);
  }

  close(...args: Parameters<SessionService["close"]>) {
    return this.#service.close(...args);
  }

  /**
   * Take one message from a gateway that is serving this session, and answer
   * with whatever frame the connection owes back.
   *
   * The socket itself belongs to the platform class, so this is the point the
   * deployed path and the dev server meet: both feed the same connection
   * handlers, and both send the reply that a settled turn earns.
   */
  async gateway(
    socket: { sessionId: string | undefined; send(frame: string): void },
    raw: string,
    now: number,
  ): Promise<void> {
    await handleSocketMessage(this.#sessions(), socket, raw, now, (settled) =>
      this.#replies(
        "text" in settled
          ? { kind: "answered", text: settled.text }
          : { kind: "failed", error: settled.error },
        settled.replyTo,
      ),
    );
    // A gateway that has just spoken may have left other turns waiting, and a
    // connection is the moment a thread learns its agent is present.
    await this.expire(socket.sessionId ?? "", now);
  }

  /** The session surface the connection handlers read. */
  #sessions(): GatewaySessions {
    return {
      session: (sessionId) => this.#service.session(sessionId),
      turns: (sessionId) => this.#service.turns(sessionId),
      transition: (sessionId, turnId, update, at) =>
        this.#service.transition(sessionId, turnId, update, at),
      bind: (sessionId, acpSessionId, gatewayId, at) =>
        this.#service.bind(sessionId, acpSessionId, gatewayId, at),
      capability: (sessionId, capability, at) =>
        this.#service.capability(sessionId, capability, at),
    };
  }
}

/**
 * Answers the thread a settled turn came from. A reply that cannot be sent is
 * swallowed rather than thrown, so a provider outage cannot undo a turn that
 * already ran.
 */
export type ReplySink = (
  outcome: { kind: "answered"; text: string } | { kind: "failed"; error: string },
  threadId: string,
) => Promise<void>;

/**
 * The reply a run's outcome earns, or `undefined` when there is nothing to say.
 *
 * A turn that never ran has no answer and no failure, so it earns no reply: an
 * empty message would read as the agent having nothing to offer.
 */
export function replyOf(
  outcome: TurnRunOutcome,
): { kind: "answered"; text: string } | { kind: "failed"; error: string } | undefined {
  if (outcome.kind === "answered") return { kind: "answered", text: outcome.text };
  if (outcome.kind === "failed") return { kind: "failed", error: outcome.error };
  return undefined;
}

export type { TurnRunOutcome };
