import { threadKey, type ThreadRef } from "@agent/store";
import { indexKey } from "./thread-index.ts";
import type { StorageHost } from "./storage.ts";

/** The thread to session mapping, over one storage host. */
export class ThreadIndexObject {
  readonly #storage: StorageHost;

  constructor(storage: StorageHost) {
    this.#storage = storage;
  }

  async lookup(thread: ThreadRef): Promise<string> {
    const key = indexKey(thread);
    const existing = await this.#storage.get<string>(key);
    const sessionId = existing ?? threadKey(thread);
    await this.#storage.put(key, sessionId);
    return sessionId;
  }

  async forget(thread: ThreadRef): Promise<void> {
    await this.#storage.delete(indexKey(thread));
  }
}
