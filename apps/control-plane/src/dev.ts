/**
 * The local control plane. `bun run dev` starts this, and the gateway dials
 * in. Secrets come from the environment so the same code runs locally and on
 * Cloudflare.
 *
 * The HTTP surface mirrors the Worker: one route per provider plus health.
 */
import { fetch as handle, type Env, type Namespace } from "./worker.ts";
import { SessionDurableObject } from "./session-do.ts";
import { ThreadIndexDurableObject } from "./thread-index-do.ts";
import type { StorageHost } from "./storage.ts";
import { mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/** Storage that survives a restart, so a session outlives `bun run dev`. */
class DiskHost implements StorageHost {
  readonly #dir: string;

  constructor(dir: string) {
    this.#dir = dir;
    mkdirSync(dir, { recursive: true });
  }

  #path(key: string): string {
    return join(this.#dir, `${encodeURIComponent(key)}.json`);
  }

  async get<T>(key: string): Promise<T | undefined> {
    try {
      return JSON.parse(readFileSync(this.#path(key), "utf8")) as T;
    } catch {
      return undefined;
    }
  }

  async put<T>(key: string, value: T): Promise<void> {
    writeFileSync(this.#path(key), JSON.stringify(value));
  }

  async delete(key: string): Promise<void> {
    try {
      unlinkSync(this.#path(key));
    } catch {
      /* the file was already gone */
    }
  }

  async list<T>(options: { prefix: string }): Promise<Map<string, T>> {
    const { readdirSync } = await import("node:fs");
    const out = new Map<string, T>();
    for (const file of readdirSync(this.#dir)) {
      const key = decodeURIComponent(file.replace(/\.json$/, ""));
      if (!key.startsWith(options.prefix)) continue;
      const value = await this.get<T>(key);
      if (value !== undefined) out.set(key, value);
    }
    return out;
  }
}

/** A namespace that builds objects on demand from disk storage. */
function namespace(build: (host: StorageHost) => unknown, root: string): Namespace {
  return {
    getByName: (name: string) => build(new DiskHost(join(root, name))),
  };
}

const root = process.env.STATE_DIR ?? ".agent-state";
const env: Env = {
  slack: {
    signingSecret: process.env.SLACK_SIGNING_SECRET ?? "dev-slack-secret",
    botUserId: process.env.SLACK_BOT_USER_ID ?? "UBOT",
  },
  discord: { publicKey: process.env.DISCORD_PUBLIC_KEY ?? "" },
  linear: { webhookSecret: process.env.LINEAR_WEBHOOK_SECRET ?? "dev-linear-secret" },
  SESSIONS: namespace((host) => new SessionDurableObject(host), join(root, "sessions")),
  THREAD_INDEX: namespace((host) => new ThreadIndexDurableObject(host), join(root, "index")),
};

const port = Number(process.env.PORT ?? 8787);
Bun.serve({
  port,
  fetch: (request) => handle(request, env),
});

console.log(`control plane listening on http://localhost:${port}`);
console.log(`state in ${root}; point the gateway at ws://localhost:${port}/gateway`);
export { DiskHost };
