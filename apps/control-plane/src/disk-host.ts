import type { StorageHost } from "./storage.ts";
import { mkdirSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/**
 * The JSON value a file holds, or `undefined` when the file is missing,
 * unreadable, or not JSON. A corrupt file counts as absent: this class is the
 * only writer, so a broken file is a crash, not a value.
 */
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

/**
 * Storage that survives a restart, so a session outlives `bun run dev`. Each
 * object gets its own directory, and each key becomes one JSON file.
 */
export class DiskHost implements StorageHost {
  readonly #dir: string;

  constructor(dir: string) {
    this.#dir = dir;
    mkdirSync(dir, { recursive: true });
  }

  /** The file a key lives in. The key is URI-encoded, so a colon stays inside one file name. */
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
