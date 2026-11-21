import type { Storage } from "@agent/store";

/** The operations a Durable Object's storage exposes that this adapter uses. */
export interface StorageHost {
  get<T>(key: string): Promise<T | undefined>;
  put<T>(key: string, value: T): Promise<void>;
  delete(key: string): Promise<void>;
  list<T>(options: { prefix: string }): Promise<Map<string, T>>;
}

/**
 * A Durable Object's storage as the repository's `Storage`. The repository is
 * written against the interface, so it never imports a runtime type.
 */
export function durableStorage(host: StorageHost): Storage {
  return {
    get: <T>(key: string) => host.get<T>(key),
    put: (key, value) => host.put(key, value),
    delete: (key) => host.delete(key),
    list: async <T>(prefix: string) => {
      const entries = await host.list<T>({ prefix });
      return [...entries].map(([key, value]) => ({ key, value }));
    },
  };
}
