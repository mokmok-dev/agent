import type { StorageHost } from "./storage.ts";
import { mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function readJson<T>(path: string): T | undefined {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return undefined;
  }
  try {
    return JSON.parse(text) as T;
  } catch {
    return undefined;
  }
}

export class DiskHost implements StorageHost {
  readonly #dir: string;

  constructor(dir: string) {
    this.#dir = dir;
    mkdirSync(dir, { recursive: true });
  }

  pathFor(key: string): string {
    return join(this.#dir, `${encodeURIComponent(key)}.json`);
  }

  async get<T>(key: string): Promise<T | undefined> {
    return readJson<T>(this.pathFor(key));
  }

  async put<T>(key: string, value: T): Promise<void> {
    writeFileSync(this.pathFor(key), JSON.stringify(value));
  }

  async delete(key: string): Promise<void> {
    try {
      unlinkSync(this.pathFor(key));
    } catch {
      // The key was already gone, which is the state the caller wants.
    }
  }

  async list<T>(options: { prefix: string }): Promise<Map<string, T>> {
    const out = new Map<string, T>();
    for (const file of readdirSync(this.#dir)) {
      const key = decodeURIComponent(file.slice(0, -".json".length));
      if (!key.startsWith(options.prefix)) continue;
      const value = readJson<T>(this.pathFor(key));
      if (value !== undefined) out.set(key, value);
    }
    return out;
  }
}
