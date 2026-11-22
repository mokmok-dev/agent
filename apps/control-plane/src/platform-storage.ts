import type { StorageHost } from "./storage.ts";

/** The slice of the Durable Object storage a session uses. */
export interface DurableStorage {
  get<T>(key: string): Promise<T | undefined>;
  put<T>(key: string, value: T): Promise<void>;
  delete(key: string): Promise<boolean>;
  list<T>(options?: { prefix?: string }): Promise<Map<string, T>>;
}

/**
 * The platform storage as the host the packages take. A Durable Object's
 * storage is the only durable store this Worker has, so every key lands there.
 */
export function objectStorage(storage: DurableStorage): StorageHost {
  return {
    get: (key) => storage.get(key),
    put: (key, value) => storage.put(key, value),
    delete: async (key) => {
      await storage.delete(key);
    },
    list: (options) => storage.list(options),
  };
}
