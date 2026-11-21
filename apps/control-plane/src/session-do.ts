import { SessionService } from "./session-service.ts";
import { durableStorage, type StorageHost } from "./storage.ts";

/**
 * One conversation's Durable Object. Cloudflare serializes calls to one
 * object, so this class needs no lock of its own: every delivery for a thread
 * observes the write the previous one made.
 */
export class SessionDurableObject {
  readonly #service: SessionService;

  constructor(storageHost: StorageHost) {
    this.#service = new SessionService(durableStorage(storageHost));
  }

  deliver(...args: Parameters<SessionService["deliver"]>) {
    return this.#service.deliver(...args);
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
