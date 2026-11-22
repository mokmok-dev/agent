import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { openDurableObjectSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/cloudflare";
import { createRegistry } from "@earendil-works/pi-durable";
import { createModels } from "@earendil-works/pi-ai";
import { cloudflareWorkersAIProvider } from "@earendil-works/pi-ai/providers/cloudflare-workers-ai";
import { ConversationBase } from "@agent/conversation";
import type { Env } from "./worker.ts";
import { piTurnRunner } from "./pi-turn-runner.ts";
import type { TurnRunner } from "./turn-runner.ts";

/**
 * The pi runner a Durable Object supplies.
 *
 * Pi stores its conversation in the same SQLite the session lives in, so the
 * turn's history survives an eviction with the session record. The model
 * collection resolves its credential from the Worker's own environment, which
 * is how pi's Cloudflare provider expects to reach a key.
 *
 * This is the one integration that needs a real deployment to prove: workerd
 * does not start on the development host, and `cloudflare:workers` is not
 * importable outside it.
 */
export function piRunnerFor(state: DurableObjectState, env: Env): TurnRunner {
  return piRunnerOver(() => open(state, env));
}

/**
 * The runner over a conversation factory. Opening is memoized, so a second turn
 * of the same session reuses the conversation it already opened: the transcript
 * is one history, and reopening would lose the in-memory half of it.
 */
export function piRunnerOver(openOnce: () => Promise<ConversationBase>): TurnRunner {
  let opened: Promise<ConversationBase> | undefined;
  return piTurnRunner({
    conversation: () => {
      opened ??= openOnce();
      return opened;
    },
  });
}

export async function open(state: DurableObjectState, env: Env): Promise<ConversationBase> {
  const storage = await openDurableObjectSqliteStorage(state.storage);
  return ConversationBase.open(storage, {
    models: modelsFor(env),
    registry: createRegistry(),
    context: BACKGROUND_CONTEXT,
  });
}

/**
 * The model collection. The provider reads `CLOUDFLARE_API_KEY` and
 * `CLOUDFLARE_ACCOUNT_ID` through this context, so the Worker's secrets are the
 * only place a key lives.
 */
export function modelsFor(env: Env) {
  const models = createModels({
    authContext: {
      env: async (name: string) => env[name as keyof Env] as string | undefined,
      fileExists: async () => false,
    },
  });
  models.setProvider(cloudflareWorkersAIProvider());
  return models;
}
