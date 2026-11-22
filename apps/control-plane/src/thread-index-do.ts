import { threadKey, type ThreadRef } from "@agent/store";
import { indexKey } from "./thread-index.ts";
import type { StorageHost } from "./storage.ts";

/** Every session seen for a provider, newest first, capped so the list stays a hint. */
const sessionsKey = (provider: ThreadRef["provider"]): string => `sessions:${provider}`;
const MAX_SESSIONS = 25;

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

  /**
   * Note that a delivery created this session, newest first.
   *
   * A Linear agent session id is made by Linear at delegation time, so a
   * gateway cannot name the session it will serve. This list is how one finds
   * the session that appeared after it connected. A repeat moves to the front,
   * because what matters is the most recent delivery, not the first ever seen.
   */
  async remember(thread: ThreadRef, sessionId: string): Promise<void> {
    const key = sessionsKey(thread.provider);
    const existing = (await this.#storage.get<string[]>(key)) ?? [];
    const next = [sessionId, ...existing.filter((id) => id !== sessionId)].slice(0, MAX_SESSIONS);
    await this.#storage.put(key, next);
  }

  /** The sessions a provider has seen, newest first. */
  async recent(provider: ThreadRef["provider"]): Promise<string[]> {
    return (await this.#storage.get<string[]>(sessionsKey(provider))) ?? [];
  }
}
