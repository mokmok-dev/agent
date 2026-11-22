import { SessionService } from "./session-service.ts";
import { durableStorage, type StorageHost } from "./storage.ts";
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
   * Record a delivery and run its turn when this object hosts that turn's
   * agent. A `stdio` session is left queued for its gateway, which is what
   * makes a VM-routed delivery reach the VM rather than a local pi loop.
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
    if (!this.#hostsTurn(outcome.selection)) return outcome;
    if (this.#runner === undefined) return outcome;
    const run = await this.#service.runNext(sessionId, this.#runner, now);
    return { ...outcome, run };
  }

  /**
   * Whether this object runs the turn, which only a `durable` route does. A
   * `stdio` route is a process the gateway supervises, so the turn waits there.
   */
  #hostsTurn(selection: { protocol: string }): boolean {
    return selection.protocol === "durable";
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
