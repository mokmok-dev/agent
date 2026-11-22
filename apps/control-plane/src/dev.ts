import { fetch as handle, type Env, type Namespace } from "./worker.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { DiskHost } from "./disk-host.ts";
import type { StorageHost } from "./storage.ts";
import { join } from "node:path";

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
  SESSIONS: namespace((host) => new SessionObject(host), join(root, "sessions")),
  THREAD_INDEX: namespace((host) => new ThreadIndexObject(host), join(root, "index")),
};

export function serve(port: number): void {
  Bun.serve({ port, fetch: (request) => handle(request, env) });
  console.log(`control plane listening on http://localhost:${port}`);
  console.log(`state in ${root}; point the gateway at ws://localhost:${port}/gateway`);
}

if (import.meta.main) serve(Number(process.env.PORT ?? 8787));
