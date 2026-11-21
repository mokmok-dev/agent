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
import { DiskHost } from "./disk-host.ts";
import type { StorageHost } from "./storage.ts";
import { join } from "node:path";

/** A namespace that builds objects on demand from disk storage. */
function namespace(build: (host: StorageHost) => unknown, root: string): Namespace {
  return { getByName: (name: string) => build(new DiskHost(join(root, name))) };
}

const root = process.env.STATE_DIR ?? ".agent-state";
export const env: Env = {
  slack: {
    signingSecret: process.env.SLACK_SIGNING_SECRET ?? "dev-slack-secret",
    botUserId: process.env.SLACK_BOT_USER_ID ?? "UBOT",
  },
  discord: { publicKey: process.env.DISCORD_PUBLIC_KEY ?? "" },
  linear: { webhookSecret: process.env.LINEAR_WEBHOOK_SECRET ?? "dev-linear-secret" },
  SESSIONS: namespace((host) => new SessionDurableObject(host), join(root, "sessions")),
  THREAD_INDEX: namespace((host) => new ThreadIndexDurableObject(host), join(root, "index")),
};

export function serve(port: number): void {
  Bun.serve({ port, fetch: (request) => handle(request, env) });
  console.log(`control plane listening on http://localhost:${port}`);
  console.log(`state in ${root}; point the gateway at ws://localhost:${port}/gateway`);
}

if (import.meta.main) serve(Number(process.env.PORT ?? 8787));
