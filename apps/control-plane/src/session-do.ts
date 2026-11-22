import { SessionService } from "./session-service.ts";
import { durableStorage, type StorageHost } from "./storage.ts";
import type { TurnRunner } from "./turn-runner.ts";

/**
 * One object per conversation. It takes a `StorageHost` and its collaborators,
 * so a test drives the real object and the platform entry only adapts the
 * platform to it.
 *
 * A runner is optional because the two runtimes differ: the Worker supplies a
 * pi runner over this object's own storage, and a deployment with no agent
 * configured leaves it out so a delivery is recorded and nothing claims to run.
 */
export class SessionObject {
  readonly #service: SessionService;
  readonly #runner: TurnRunner | undefined;

  constructor(
    storageHost: StorageHost,
    deps?: ConstructorParameters<typeof SessionService>[1],
    runner?: TurnRunner,
  ) {
    this.#service = new SessionService(durableStorage(storageHost), deps);
    this.#runner = runner;
  }

  deliver(...args: Parameters<SessionService["deliver"]>) {
    return this.#service.deliver(...args);
  }

  /**
   * Record a delivery and run the turn it created, so the caller can answer
   * with the agent's words. A refused delivery runs nothing, and a deployment
   * with no runner records the turn and reports that it did not run.
   */
  async accept(
    sessionId: string,
    thread: Parameters<SessionService["deliver"]>[1],
    prompt: string,
    idempotencyKey: string,
    now: number,
  ): Promise<unknown> {
    const outcome = await this.#service.deliver(sessionId, thread, prompt, idempotencyKey, now);
    // A refused delivery has no queued turn, so nothing runs and the refusal is
    // returned as it is; every other delivery runs the turn it created.
    if (outcome.kind === "refused") return outcome;
    if (this.#runner === undefined) return outcome;
    const run = await this.#service.runNext(sessionId, this.#runner, now);
    return { ...outcome, run };
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

  close(...args: Parameters<SessionService["close"]>) {
    return this.#service.close(...args);
  }
}
