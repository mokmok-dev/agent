import { describe, expect, test } from "bun:test";
import { objectStorage, type DurableStorage } from "./platform-storage.ts";

/** The Durable Object storage contract, as the platform presents it. */
class MemoryObjectStorage implements DurableStorage {
  readonly values = new Map<string, string>();

  async get<T>(key: string): Promise<T | undefined> {
    const raw = this.values.get(key);
    return raw === undefined ? undefined : (JSON.parse(raw) as T);
  }

  async put<T>(key: string, value: T): Promise<void> {
    this.values.set(key, JSON.stringify(value));
  }

  async delete(key: string): Promise<boolean> {
    return this.values.delete(key);
  }

  async list<T>(options?: { prefix?: string }): Promise<Map<string, T>> {
    return new Map(
      [...this.values.entries()]
        .filter(([key]) => key.startsWith(options?.prefix ?? ""))
        .map(([key, raw]) => [key, JSON.parse(raw) as T]),
    );
  }
}

describe("the platform storage adapter", () => {
  test("reads, writes, and deletes through the object storage", async () => {
    const platform = new MemoryObjectStorage();
    const host = objectStorage(platform);
    await host.put("session:s1", { id: "s1" });
    expect(await host.get("session:s1")).toEqual({ id: "s1" });
    await host.delete("session:s1");
    expect(await host.get("session:s1")).toBeUndefined();
  });

  test("deleting a key that is gone still resolves, because the caller wants it gone", async () => {
    const host = objectStorage(new MemoryObjectStorage());
    expect(await host.delete("missing")).toBeUndefined();
  });

  test("lists by prefix and reports an empty map when nothing matches", async () => {
    const platform = new MemoryObjectStorage();
    const host = objectStorage(platform);
    await host.put("turn:s1:t1", { id: "t1" });
    await host.put("turn:s2:t1", { id: "other" });
    const listed = await host.list<{ id: string }>({ prefix: "turn:s1:" });
    expect([...listed]).toEqual([["turn:s1:t1", { id: "t1" }]]);
    expect((await host.list({ prefix: "nothing:" })).size).toBe(0);
  });

  test("passes the prefix to the platform as its prefix option", async () => {
    const prefixes: (string | undefined)[] = [];
    const host = objectStorage({
      get: async () => undefined,
      put: async () => undefined,
      delete: async () => false,
      list: async (options) => {
        prefixes.push(options?.prefix);
        return new Map();
      },
    });
    await host.list({ prefix: "abc" });
    expect(prefixes).toEqual(["abc"]);
  });
});
