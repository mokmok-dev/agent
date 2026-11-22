import { SessionService } from "./session-service.ts";
import { durableStorage, type StorageHost } from "./storage.ts";

/**
 * One object per conversation. It takes a `StorageHost` and its collaborators,
 * so a test drives the real object and `durable-objects.ts` only adapts the
 * platform to it.
 */
export class SessionObject {
  readonly #service: SessionService;

  constructor(storageHost: StorageHost, deps?: ConstructorParameters<typeof SessionService>[1]) {
    this.#service = new SessionService(durableStorage(storageHost), deps);
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
