import { DurableObject } from "cloudflare:workers";
import type { ThreadRef } from "@agent/store";
import { objectStorage } from "./platform-storage.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { fetch as handle, type Env } from "./worker.ts";

/**
 * The entry Wrangler deploys. A Durable Object's RPC is class-based, so each
 * class carries the platform's constructor and storage adapter; the logic is in
 * `SessionObject` and `ThreadIndexObject`, which a test drives without the
 * platform. This module is the only one that imports `cloudflare:workers`.
 */
export class SessionDurableObject extends DurableObject<Env> {
  readonly #object: SessionObject;

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.#object = new SessionObject(objectStorage(state.storage));
  }

  deliver(...args: Parameters<SessionObject["deliver"]>) {
    return this.#object.deliver(...args);
  }

  session(...args: Parameters<SessionObject["session"]>) {
    return this.#object.session(...args);
  }

  turns(...args: Parameters<SessionObject["turns"]>) {
    return this.#object.turns(...args);
  }

  next(...args: Parameters<SessionObject["next"]>) {
    return this.#object.next(...args);
  }

  active(...args: Parameters<SessionObject["active"]>) {
    return this.#object.active(...args);
  }

  transition(...args: Parameters<SessionObject["transition"]>) {
    return this.#object.transition(...args);
  }

  bind(...args: Parameters<SessionObject["bind"]>) {
    return this.#object.bind(...args);
  }

  close(...args: Parameters<SessionObject["close"]>) {
    return this.#object.close(...args);
  }
}

export class ThreadIndexDurableObject extends DurableObject<Env> {
  readonly #object: ThreadIndexObject;

  constructor(state: DurableObjectState, env: Env) {
    super(state, env);
    this.#object = new ThreadIndexObject(objectStorage(state.storage));
  }

  lookup(thread: ThreadRef): Promise<string> {
    return this.#object.lookup(thread);
  }

  forget(thread: ThreadRef): Promise<void> {
    return this.#object.forget(thread);
  }
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },
};
