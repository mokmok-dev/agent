import { fetch as handle, type Env, type Namespace } from "./worker.ts";
import { SessionObject } from "./session-do.ts";
import { ThreadIndexObject } from "./thread-index-do.ts";
import { LinearOAuthObject } from "./linear-oauth-do.ts";
import { DiskHost } from "./disk-host.ts";
import type { StorageHost } from "./storage.ts";
import { join } from "node:path";

function namespace(build: (host: StorageHost) => unknown, root: string): Namespace {
  return { getByName: (name: string) => build(new DiskHost(join(root, name))) };
}

const root = process.env.STATE_DIR ?? ".agent-state";
export const env: Env = {
  SLACK_SIGNING_SECRET: process.env.SLACK_SIGNING_SECRET ?? "dev-slack-secret",
  SLACK_BOT_USER_ID: process.env.SLACK_BOT_USER_ID ?? "UBOT",
  DISCORD_PUBLIC_KEY: process.env.DISCORD_PUBLIC_KEY ?? "",
  LINEAR_WEBHOOK_SECRET: process.env.LINEAR_WEBHOOK_SECRET ?? "dev-linear-secret",
  LINEAR_CLIENT_ID: process.env.LINEAR_CLIENT_ID ?? "",
  LINEAR_CLIENT_SECRET: process.env.LINEAR_CLIENT_SECRET ?? "",
  LINEAR_REDIRECT_URI:
    process.env.LINEAR_REDIRECT_URI ?? "http://localhost:8787/oauth/linear/callback",
  SESSIONS: namespace((host) => new SessionObject(host), join(root, "sessions")),
  THREAD_INDEX: namespace((host) => new ThreadIndexObject(host), join(root, "index")),
  LINEAR_OAUTH: namespace(
    (host) =>
      new LinearOAuthObject(host, {
        clientId: process.env.LINEAR_CLIENT_ID ?? "",
        clientSecret: process.env.LINEAR_CLIENT_SECRET ?? "",
        redirectUri: process.env.LINEAR_REDIRECT_URI ?? "",
      }),
    join(root, "linear"),
  ),
};

export function serve(port: number): void {
  Bun.serve({ port, fetch: (request) => handle(request, env) });
  console.log(`control plane listening on http://localhost:${port}`);
  console.log(`state in ${root}; point the gateway at ws://localhost:${port}/gateway`);
}

if (import.meta.main) serve(Number(process.env.PORT ?? 8787));
