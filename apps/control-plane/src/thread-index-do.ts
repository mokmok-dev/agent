import { indexKey, sessionIdFor } from "./thread-index.ts";
import type { ThreadRef } from "@agent/store";
import type { StorageHost } from "./storage.ts";

/**
 * The thread index: which session a conversation already owns. It is a
 * Durable Object of its own so two deliveries for one thread, which may land
 * on different session objects, still agree on the session id.
 */
export class ThreadIndexDurableObject {
  readonly #storage: StorageHost;

  constructor(storage: StorageHost) {
    this.#storage = storage;
  }

  /**
   * The session id for a thread, assigning it on first use. A Durable Object
   * serializes calls, so two concurrent deliveries cannot both assign.
   */
  async lookup(thread: ThreadRef): Promise<string> {
    const key = indexKey(thread);
    const existing = await this.#storage.get<string>(key);
    const sessionId = existing ?? sessionIdFor(thread);
    await this.#storage.put(key, sessionId);
    return sessionId;
  }

  /** Release a thread, so the next delivery starts a new session. */
  async forget(thread: ThreadRef): Promise<void> {
    await this.#storage.delete(indexKey(thread));
  }
}
